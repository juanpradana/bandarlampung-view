//! Upstream stream proxying.
//!
//! Both upstreams gate their video behind something a browser cannot supply:
//!
//! * **Seribu Wajah** returns `403 Akses Ditolak: Hotlinking` unless the request
//!   carries a `Referer`/`Origin` on `seribuwajah.bandarlampungkota.go.id`. A
//!   browser can never set those for a cross-origin `<video>`, so we fetch
//!   server-side and re-serve with permissive CORS.
//! * **Polresta** serves a signed HLS URL out of `rtsp.itn.net.id`, whose resolve
//!   endpoint only answers for a whitelisted `parent_origin`. We call it with the
//!   Polresta origin, which is allowed, and hand the ticket back to our client.
//!
//! Playlists are rewritten so every relative reference points back at our own
//! `/stream/…` path, keeping the whole HLS session behind one origin.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use tracing::{debug, warn};

use cctv_web_core::{Camera, Playback, Source};

use crate::state::{AppState, PolSession};

/// Upstream host serving the Seribu Wajah HLS tree.
const SW_STREAM_HOST: &str = "https://seribuwajah.bandarlampungkota.go.id";
/// Origin/Referer Seribu Wajah's hotlink guard demands.
const SW_REFERER: &str = "https://seribuwajah.bandarlampungkota.go.id/map";
const SW_ORIGIN: &str = "https://seribuwajah.bandarlampungkota.go.id";

/// Pause before the single retry of a transiently-failing upstream playlist.
///
/// Long enough for an RTSP source to finish re-authenticating, short enough
/// that a player waiting on the manifest is not stalled noticeably.
const SW_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

/// Polresta origin — without trailing path (RFC 6454 serialized origin).
const RESTA_ORIGIN: &str = "https://restabandarlampung.lampung.polri.go.id";
const RESTA_REFERER: &str = "https://restabandarlampung.lampung.polri.go.id/peta";
/// ITN gateway that mints signed HLS URLs for Polresta cameras.
const ITN_RESOLVE: &str = "https://rtsp.itn.net.id/api/v1/public/embed";

/// How long a resolved Polresta session is trusted before we mint a fresh one.
///
/// The gateway mints a ticket with a hard 60 s expiry (the `exp` claim inside
/// the ticket is always `now + 60`), so trusting a cached session for longer
/// than that guarantees the tail end of every window serves 403s. 45 s leaves
/// a 15 s safety margin for clock skew and in-flight segment fetches.
const POL_SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(45);

pub struct ProxyError(StatusCode, String);

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

fn err(code: StatusCode, msg: impl Into<String>) -> ProxyError {
    ProxyError(code, msg.into())
}

/// Forward an upstream status when a client could meaningfully receive it.
///
/// Only 401/403 survive verbatim: those tell the caller their credential is
/// dead, which is exactly the signal the Polresta session logic keys off.
/// Everything else (including the gateway's 503 "no segments available yet"
/// warm-up and its 404 "segment not ready") is an upstream implementation
/// detail, so it collapses to 502 rather than leaking a misleading status.
fn upstream_status(status: reqwest::StatusCode) -> StatusCode {
    match status.as_u16() {
        401 | 403 => {
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY)
        }
        _ => StatusCode::BAD_GATEWAY,
    }
}

/// Statuses a client could actually act on from the Seribu Wajah proxy.
///
/// 404 is genuine — the segment window is short, so a stale playlist reference
/// really is gone. Everything else — including the 401 the RTSP source emits
/// while re-authenticating and the 500 "muxer instance not available" the
/// origin returns for a camera whose encoder is down — is an upstream
/// implementation detail we must not leak as-is.
fn sw_upstream_status(status: reqwest::StatusCode) -> StatusCode {
    match status.as_u16() {
        404 => StatusCode::NOT_FOUND,
        _ => StatusCode::BAD_GATEWAY,
    }
}

/// Whether a Seribu Wajah upstream status is worth one quick retry.
///
/// The origin hands back 401 while the RTSP source re-authenticates and 500
/// while a muxer is still attaching; both clear within a second or so. A 404
/// does not: a missing path or expired segment is a permanent answer.
fn is_sw_transient(status: reqwest::StatusCode) -> bool {
    matches!(
        status.as_u16(),
        401 | 403 | 500 | 502 | 503 | 504 | 429
    )
}

/// Look up a camera and confirm it can be proxied as HLS.
fn hls_camera(state: &AppState, id: &str) -> Result<Camera, ProxyError> {
    let cam = state
        .dataset()
        .find(id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown camera"))?
        .clone();

    // Seribu Wajah cameras have playback == Hls; Polresta cameras carry a token
    // that resolves into an HLS stream on the ITN gateway.
    let playable = cam.playback == Playback::Hls
        || (cam.source == Source::Polresta && cam.token.is_some());

    if !playable {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "camera cannot be proxied as an HLS stream",
        ));
    }
    Ok(cam)
}

/// `GET /api/cameras/:id/stream` — the HLS entry playlist.
///
/// Seribu Wajah cameras map straight onto a static upstream path. Polresta
/// cameras need a resolved, IP-bound gateway session first.
pub async fn stream_entry(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ProxyError> {
    let cam = hls_camera(&state, &id)?;

    match cam.source {
        Source::SeribuWajah => {
            let path = format!("cctv_{}/index.m3u8", cam.source_key);
            serve_playlist(&state, &cam, &path).await
        }
        Source::Polresta => serve_pol_entry(&state, &cam).await,
    }
}

/// `GET /api/cameras/:id/stream/{*asset}` — sub-playlists and media segments.
pub async fn stream_asset(
    State(state): State<AppState>,
    Path((id, asset)): Path<(String, String)>,
) -> Result<Response, ProxyError> {
    let cam = hls_camera(&state, &id)?;

    // Reject traversal outright rather than relying on the upstream to.
    if asset.contains("..") || asset.starts_with('/') {
        return Err(err(StatusCode::BAD_REQUEST, "invalid asset path"));
    }

    match cam.source {
        Source::SeribuWajah => {
            let path = format!("cctv_{}/{}", cam.source_key, asset);
            if asset.ends_with(".m3u8") {
                serve_playlist(&state, &cam, &path).await
            } else {
                serve_segment(&state, &cam, &path).await
            }
        }
        Source::Polresta => serve_pol_asset(&state, &cam, &asset).await,
    }
}

/// Fetch an upstream playlist, rewriting all references to our own paths.
///
/// A transient upstream rejection (source re-authenticating, muxer still
/// attaching) gets exactly one retry after a short pause: the browser would
/// otherwise tear the player down over a hiccup the origin resolves itself in
/// well under a second.
async fn serve_playlist(
    state: &AppState,
    cam: &Camera,
    upstream_path: &str,
) -> Result<Response, ProxyError> {
    let url = format!("{SW_STREAM_HOST}/{upstream_path}");

    let mut last: Option<(StatusCode, String)> = None;

    for attempt in 0..2 {
        let resp = state
            .http()
            .get(&url)
            .header(header::REFERER, SW_REFERER)
            .header(header::ORIGIN, SW_ORIGIN)
            .header(header::ACCEPT, "*/*")
            .send()
            .await
            .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("upstream: {e}")))?;

        let status = resp.status();
        if status.is_success() {
            let text = resp
                .text()
                .await
                .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("read playlist: {e}")))?;
            return Ok(playlist_response(rewrite_playlist(
                &text,
                &cam.id,
                upstream_path,
            )));
        }

        let body = resp.text().await.unwrap_or_default();
        warn!(%status, %url, attempt, "playlist upstream error: {body}");

        if attempt == 0 && is_sw_transient(status) {
            tokio::time::sleep(SW_RETRY_DELAY).await;
            last = Some((status, body));
            continue;
        }

        return Err(err(
            sw_upstream_status(status),
            format!("upstream returned {status}"),
        ));
    }

    // Unreachable in practice: the loop either returns on success or on the
    // non-retryable / final attempt. Kept so the type checker sees the exit.
    let (status, _) = last.unwrap_or((reqwest::StatusCode::BAD_GATEWAY, String::new()));
    Err(err(
        sw_upstream_status(status),
        format!("upstream returned {status}"),
    ))
}

/// Stream a media segment straight through, preserving Range semantics.
async fn serve_segment(
    state: &AppState,
    _cam: &Camera,
    upstream_path: &str,
) -> Result<Response, ProxyError> {
    let url = format!("{SW_STREAM_HOST}/{upstream_path}");

    let resp = state
        .http()
        .get(&url)
        .header(header::REFERER, SW_REFERER)
        .header(header::ORIGIN, SW_ORIGIN)
        .header(header::ACCEPT, "*/*")
        .send()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("upstream: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        // No retry here: hls.js re-requests the live edge on its own next
        // playlist poll, so a retry only doubles latency on a dead segment.
        return Err(err(
            sw_upstream_status(status),
            format!("upstream returned {status}"),
        ));
    }

    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("video/mp2t")
        .to_string();

    // Media segments are immutable once written — cache them hard.
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("video/mp2t")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=60"),
    );

    let stream = resp
        .bytes_stream()
        .map(|chunk| chunk.map_err(|e| std::io::Error::other(e.to_string())));

    Ok((headers, Body::from_stream(stream)).into_response())
}

/// Rewrite a playlist so every relative URI resolves through our proxy.
///
/// Non-comment lines are URIs (absolute, root-relative, or relative to the
/// playlist). `#EXT-X-KEY` / `#EXT-X-MAP` carry `URI="…"` attributes that need
/// the same treatment.
fn rewrite_playlist(text: &str, camera_id: &str, upstream_path: &str) -> String {
    // Directory of the current playlist, for resolving relative siblings.
    let base_dir = match upstream_path.rfind('/') {
        Some(i) => &upstream_path[..=i],
        None => "",
    };

    let mut out = String::with_capacity(text.len() + 256);

    for line in text.lines() {
        if line.is_empty() {
            out.push('\n');
            continue;
        }

        if let Some(rest) = line.strip_prefix('#') {
            // Rewrite URI="…" inside tag attributes.
            if rest.contains("URI=\"") {
                out.push_str(&rewrite_tag_uri(line, camera_id, base_dir));
            } else {
                out.push_str(line);
            }
            out.push('\n');
            continue;
        }

        // A bare URI line.
        out.push_str(&proxy_url(camera_id, base_dir, line.trim()));
        out.push('\n');
    }

    out
}

/// Rewrite the `URI="…"` attribute of an HLS tag line.
fn rewrite_tag_uri(line: &str, camera_id: &str, base_dir: &str) -> String {
    const MARK: &str = "URI=\"";
    let Some(start) = line.find(MARK) else {
        return line.to_string();
    };
    let value_start = start + MARK.len();
    let Some(end_rel) = line[value_start..].find('"') else {
        return line.to_string();
    };
    let value_end = value_start + end_rel;

    let uri = &line[value_start..value_end];
    let rewritten = proxy_url(camera_id, base_dir, uri);

    format!(
        "{}{}{}",
        &line[..value_start],
        rewritten,
        &line[value_end..]
    )
}

/// Turn one playlist reference into a path on our own origin.
fn proxy_url(camera_id: &str, base_dir: &str, uri: &str) -> String {
    // Absolute URLs pointing at the upstream host → strip to a path.
    let path = if let Some(rest) = uri.strip_prefix(SW_STREAM_HOST) {
        // e.g. https://host/cctv_19/main_stream.m3u8 -> cctv_19/main_stream.m3u8
        rest.trim_start_matches('/').to_string()
    } else if let Some(rest) = uri.strip_prefix("//") {
        // protocol-relative
        rest.split_once('/').map(|(_, p)| p.to_string()).unwrap_or_default()
    } else if uri.starts_with('/') {
        uri.trim_start_matches('/').to_string()
    } else {
        // Relative to the playlist's directory.
        format!("{base_dir}{uri}")
    };

    // Strip the `cctv_{key}/` prefix — our route re-adds it.
    let asset = path
        .split_once('/')
        .map(|(_, tail)| tail.to_string())
        .unwrap_or(path);

    format!("/api/cameras/{camera_id}/stream/{asset}")
}

// ------------------------------------------------------------------ Polresta

/// Resolve a fresh gateway session for a Polresta camera.
///
/// The endpoint is gated on the caller's `Origin`; the Polresta web origin is
/// whitelisted, so we present it on the browser's behalf. The ticket we get back
/// is bound to this server's IP, which is exactly why the browser must never see
/// it — it could not use it anyway.
///
/// The embed token is the *last path segment* of `embed_url`, e.g.
/// `https://rtsp.itn.net.id/embed/u7u9h4k42z` → `u7u9h4k42z`. The `subtitle`
/// field holds a different, unrelated `cam_...` id and is rejected by the gate.
async fn resolve_pol_session(
    state: &AppState,
    cam: &Camera,
) -> Result<PolSession, ProxyError> {
    let embed = cam
        .embed_url
        .as_deref()
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "camera missing embed URL"))?;

    let token = embed
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "embed URL has no token"))?;

    let url = format!("{ITN_RESOLVE}/{token}/resolve");

    let resp = state
        .http()
        .get(&url)
        .header(header::ACCEPT, "application/json")
        // The gate is the Origin header, not the query parameter: any explicit
        // `parent_origin` value other than Polresta's own is rejected outright.
        .header(header::ORIGIN, RESTA_ORIGIN)
        .header(header::REFERER, RESTA_REFERER)
        .send()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("upstream: {e}")))?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("read resolve: {e}")))?;

    if !status.is_success() {
        warn!(%status, "Polresta resolve rejected");
        return Err(err(StatusCode::BAD_GATEWAY, "stream access denied upstream"));
    }

    let master = body
        .pointer("/data/hls_stream_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "resolve returned no HLS URL"))?;

    Ok(session_from_master(master))
}

/// Split a signed master URL into the reusable base directory + ticket query.
///
/// e.g. `https://bdl01.rtsp.itn.net.id/stream/{cam}/channel/0/hls/{sid}/master.m3u8?ticket=...`
/// becomes base `https://bdl01.rtsp.itn.net.id/stream/{cam}/channel/0/hls/{sid}/`
/// and ticket `?ticket=...`.
fn session_from_master(master: &str) -> PolSession {
    let (path, query) = master.split_once('?').unwrap_or((master, ""));
    let base = match path.rfind('/') {
        Some(i) => path[..=i].to_string(),
        None => path.to_string(),
    };
    PolSession {
        base,
        ticket: if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        },
        resolved_at: std::time::Instant::now(),
    }
}

/// Return a session that is still within its trust window, minting one if needed.
async fn pol_session(state: &AppState, cam: &Camera) -> Result<PolSession, ProxyError> {
    if let Some(s) = state.pol_session(&cam.id) {
        if s.resolved_at.elapsed() < POL_SESSION_TTL {
            return Ok(s);
        }
    }
    let fresh = resolve_pol_session(state, cam).await?;
    state.set_pol_session(&cam.id, fresh.clone());
    Ok(fresh)
}

/// `GET /api/cameras/:id/stream` for a Polresta camera.
async fn serve_pol_entry(state: &AppState, cam: &Camera) -> Result<Response, ProxyError> {
    let mut session = pol_session(state, cam).await?;
    let master_url = format!("{}master.m3u8{}", session.base, session.ticket);

    // If the master fetch fails with 401/403, our cached ticket was revoked
    // or expired upstream; drop the cached session and retry once with a fresh one.
    let text = match fetch_text(state, &master_url).await {
        Ok(t) => t,
        Err(e) if e.0 == StatusCode::UNAUTHORIZED || e.0 == StatusCode::FORBIDDEN => {
            debug!(camera_id = %cam.id, "Polresta session stale on entry, re-resolving");
            state.clear_pol_session(&cam.id);
            session = resolve_pol_session(state, cam).await?;
            state.set_pol_session(&cam.id, session.clone());
            let retry_url = format!("{}master.m3u8{}", session.base, session.ticket);
            fetch_text(state, &retry_url).await?
        }
        Err(e) => return Err(e),
    };

    let rewritten = rewrite_pol_playlist(&text, &cam.id);
    Ok(playlist_response(rewritten))
}

/// `GET /api/cameras/:id/stream/{*asset}` for a Polresta camera.
///
/// Gateway URIs are root-relative paths that already carry their own ticket, so
/// the asset name is all we need to rebuild the upstream URL. The ticket itself
/// is restored from the cached session — the browser never sees it.
///
/// The session's trust window is deliberately *not* extended here. The signed
/// ticket carries a fixed `exp` (mint + 60 s), so a session that keeps serving
/// segments is not thereby renewed; sliding `resolved_at` forward on every fetch
/// is what let a dead ticket be trusted indefinitely and 403 forever.
async fn serve_pol_asset(
    state: &AppState,
    cam: &Camera,
    asset: &str,
) -> Result<Response, ProxyError> {
    // Artifact name only: strip any query the client tried to smuggle in.
    let name = asset.split_once('?').map(|(n, _)| n).unwrap_or(asset);
    let name = name.rsplit('/').next().unwrap_or(name);

    if !is_pol_asset(name) {
        return Err(err(StatusCode::BAD_REQUEST, "invalid asset name"));
    }

    let session = pol_session(state, cam).await?;
    if session.ticket.is_empty() {
        return Err(err(
            StatusCode::BAD_GATEWAY,
            "gateway session has no ticket",
        ));
    }

    let upstream = format!("{}{}{}", session.base, name, session.ticket);

    if !name.ends_with(".m3u8") {
        return match fetch_binary(state, &upstream).await {
            Ok(r) => Ok(r),
            Err(e) => {
                // A revoked ticket is worth dropping so the next playlist reload
                // mints a fresh one. Note a *new* session has its own directory
                // and its own segment numbering, so retrying this exact segment
                // against it would ask for a name that does not exist yet —
                // clearing and handing back the 403 lets hls.js reload the
                // playlist instead, which is the only correct recovery.
                if is_pol_auth_rejection(e.0) {
                    state.clear_pol_session(&cam.id);
                }
                Err(e)
            }
        };
    }

    // Playlists are safe to retry: we re-resolve, then re-read the (possibly
    // still warming-up) playlist from the new directory.
    let text = match fetch_text(state, &upstream).await {
        Ok(t) => t,
        Err(e) if is_pol_auth_rejection(e.0) => {
            debug!(camera_id = %cam.id, "Polresta session stale on playlist, re-resolving");
            state.clear_pol_session(&cam.id);
            let fresh = resolve_pol_session(state, cam).await?;
            let retry = format!("{}{}{}", fresh.base, name, fresh.ticket);
            state.set_pol_session(&cam.id, fresh);
            fetch_text(state, &retry).await?
        }
        // A 503 here is transient gateway warm-up ("no segments available yet")
        // and must NOT cost us the session, or we restart warm-up forever.
        Err(e) => return Err(e),
    };

    Ok(playlist_response(rewrite_pol_playlist(&text, &cam.id)))
}

/// Only the artifacts an ITN playlist actually references: media segments and
/// the init map. Anything else (e.g. a directory, or an HTML path) is refused
/// so a crafted asset name can't be turned into an arbitrary gateway request.
fn is_pol_asset(name: &str) -> bool {
    let ok_ext = [".m4s", ".mp4", ".m3u8", ".ts", ".aac"]
        .iter()
        .any(|ext| name.ends_with(ext));
    ok_ext && !name.contains('/') && !name.contains('\\') && !name.contains("..")
}

/// 401/403 = the credential itself is refused, as opposed to a transient or
/// content-level failure.
fn is_pol_auth_rejection(code: StatusCode) -> bool {
    code == StatusCode::UNAUTHORIZED || code == StatusCode::FORBIDDEN
}

/// Rewrite a Polresta playlist so children route through our own stream path.
///
/// Children are absolute paths like
/// `/stream/{cam}/channel/0/hls/{sid}/seg-0.m4s?ticket=...`.
/// We keep the filename but drop the ticket — the browser gets an opaque path and
/// the server re-attaches the ticket when it fetches.
fn rewrite_pol_playlist(text: &str, camera_id: &str) -> String {
    let mut out = String::with_capacity(text.len());

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('#') {
            if rest.contains("URI=\"") {
                out.push_str(&rewrite_pol_tag_uri(line, camera_id));
            } else {
                out.push_str(line);
            }
            out.push('\n');
            continue;
        }
        if line.trim().is_empty() {
            out.push('\n');
            continue;
        }
        out.push_str(&pol_asset_path(camera_id, line.trim()));
        out.push('\n');
    }

    out
}

fn rewrite_pol_tag_uri(line: &str, camera_id: &str) -> String {
    const MARK: &str = "URI=\"";
    let Some(start) = line.find(MARK) else {
        return line.to_string();
    };
    let value_start = start + MARK.len();
    let Some(end_rel) = line[value_start..].find('"') else {
        return line.to_string();
    };
    let value_end = value_start + end_rel;

    let rewritten = pol_asset_path(camera_id, &line[value_start..value_end]);
    format!("{}{}{}", &line[..value_start], rewritten, &line[value_end..])
}

/// Map an upstream gateway reference to `/api/cameras/{id}/stream/{file}`.
fn pol_asset_path(camera_id: &str, uri: &str) -> String {
    // Drop query (the ticket) and take just the final path segment.
    let path = uri.split_once('?').map(|(p, _)| p).unwrap_or(uri);
    let file = path.rsplit('/').next().unwrap_or(path);
    format!("/api/cameras/{camera_id}/stream/{file}")
}

/// Fetch a text body, preserving the upstream status code on failure.
///
/// The status must survive verbatim: the Polresta session logic keys off 401/403
/// to decide whether a cached ticket is dead, and collapsing every failure into
/// 502 makes that check unreachable.
async fn fetch_text(state: &AppState, url: &str) -> Result<String, ProxyError> {
    let resp = state
        .http()
        .get(url)
        .send()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("upstream: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(err(
            upstream_status(status),
            format!("upstream returned {status}"),
        ));
    }
    resp.text()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("read body: {e}")))
}

/// Stream an upstream body through, preserving the upstream status on failure.
async fn fetch_binary(state: &AppState, url: &str) -> Result<Response, ProxyError> {
    let resp = state
        .http()
        .get(url)
        .send()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("upstream: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(err(
            upstream_status(status),
            format!("upstream returned {status}"),
        ));
    }

    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=60"),
    );

    let stream = resp
        .bytes_stream()
        .map(|chunk| chunk.map_err(|e| std::io::Error::other(e.to_string())));

    Ok((headers, Body::from_stream(stream)).into_response())
}

fn playlist_response(body: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.apple.mpegurl"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    (headers, body).into_response()
}

/// Inline SVG shown when a camera has no upstream still image at all.
///
/// A 200 with a legible frame beats a 4xx on an `<img>`: the card keeps its
/// aspect box, the label still says which camera this is, and the console
/// stays quiet about a resource the API was never going to serve.
fn placeholder_image(name: &str) -> Response {
    // Escape for both XML text content and the double-quoted attribute below.
    let safe: String = name
        .chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            _ => c.to_string(),
        })
        .collect();

    let mut svg = String::from(r##"<svg xmlns="http://www.w3.org/2000/svg" width="320" height="180" viewBox="0 0 320 180" role="img">"##);
    svg.push_str(r##"<rect width="320" height="180" fill="#0a1120"/>"##);
    svg.push_str(r##"<g fill="none" stroke="#475569" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"##);
    svg.push_str(r##"<path d="M132 62h40l8 8h28a4 4 0 0 1 4 4v38a4 4 0 0 1-4 4H112a4 4 0 0 1-4-4V74a4 4 0 0 1 4-4h12z"/>"##);
    svg.push_str(r##"<path d="M112 86l24 16 16-12 20 16 16-12"/></g>"##);
    svg.push_str(r##"<text x="160" y="152" fill="#94a3b8" font-family="sans-serif" font-size="13" text-anchor="middle">"##);
    svg.push_str(&safe);
    svg.push_str(r##"</text></svg>"##);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    (headers, svg).into_response()
}

/// `GET /api/cameras/:id/snapshot` — still image, refreshed every 5 min upstream.
pub async fn snapshot(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ProxyError> {
    let cam = state
        .dataset()
        .find(&id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown camera"))?
        .clone();

    if cam.source != Source::SeribuWajah {
        // Polresta exposes no `/snapshot` upstream, only a thumbnail. Route to
        // it so a `<img>` fallback works instead of returning 400; a camera
        // with neither gets an inline placeholder instead of a broken frame.
        if cam.thumbnail.is_some() {
            return thumb(State(state), Path(id)).await;
        }
        return Ok(placeholder_image(&cam.name));
    }

    // Upstream buckets the snapshot every 300 s; match that so we share cache.
    let bucket = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 300)
        .unwrap_or(0);

    let url = format!(
        "https://api-newseribuwajah.bandarlampungkota.go.id/cctvs/public/{}/snapshot?t={bucket}",
        cam.source_key
    );

    let resp = state
        .http()
        .get(&url)
        .send()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("upstream: {e}")))?;

    if !resp.status().is_success() {
        return Err(err(StatusCode::BAD_GATEWAY, "snapshot unavailable"));
    }

    let bytes: Bytes = resp
        .bytes()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("read snapshot: {e}")))?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=30"),
    );

    Ok((headers, bytes).into_response())
}

/// `GET /api/cameras/:id/thumb` — proxy a Polresta thumbnail (hotlink-free).
pub async fn thumb(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ProxyError> {
    let cam = state
        .dataset()
        .find(&id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown camera"))?
        .clone();

    let url = cam
        .thumbnail
        .clone()
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "camera has no thumbnail"))?;

    debug!(%url, "fetching thumbnail");

    let resp = state
        .http()
        .get(&url)
        .send()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("upstream: {e}")))?;

    if !resp.status().is_success() {
        return Err(err(StatusCode::BAD_GATEWAY, "thumbnail unavailable"));
    }

    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/webp")
        .to_string();

    let bytes: Bytes = resp
        .bytes()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("read thumb: {e}")))?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("image/webp")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );

    Ok((headers, bytes).into_response())
}

/// `GET /api/cameras/:id/resolve` — confirm a Polresta camera is streamable.
///
/// The gateway's signed URL is bound to *this server's* IP, so handing it to the
/// browser would be useless (and leaks a credential). We resolve only to verify
/// availability and return our own opaque stream path instead.
pub async fn resolve_hls(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ProxyError> {
    let cam = state
        .dataset()
        .find(&id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown camera"))?
        .clone();

    if cam.source != Source::Polresta {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "only Polresta cameras need resolving",
        ));
    }

    let session = resolve_pol_session(&state, &cam).await?;
    state.set_pol_session(&cam.id, session);

    Ok(axum::Json(serde_json::json!({
        "id": cam.id,
        "stream": format!("/api/cameras/{}/stream", cam.id),
        "ready": true,
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_relative_playlist_refs() {
        let input = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=5000000\nmain_stream.m3u8\n";
        let out = rewrite_playlist(input, "sw-19", "cctv_19/index.m3u8");
        assert!(out.contains("/api/cameras/sw-19/stream/main_stream.m3u8"));
        assert!(out.contains("#EXT-X-STREAM-INF"));
    }

    #[test]
    fn rewrites_nested_playlist_refs() {
        let input = "#EXTM3U\n#EXTINF:4.0,\na2366f53439a_main_seg290.ts\n";
        let out = rewrite_playlist(input, "sw-19", "cctv_19/main_stream.m3u8");
        assert!(out.contains("/api/cameras/sw-19/stream/a2366f53439a_main_seg290.ts"));
    }

    #[test]
    fn rewrites_absolute_uri_attr() {
        let input = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"https://seribuwajah.bandarlampungkota.go.id/cctv_19/key.bin\"\n";
        let out = rewrite_playlist(input, "sw-19", "cctv_19/index.m3u8");
        assert!(out.contains("URI=\"/api/cameras/sw-19/stream/key.bin\""));
    }

    #[test]
    fn rejects_traversal() {
        // covered in the handler; the helper keeps paths inside the cctv_ dir
        let out = proxy_url("sw-19", "cctv_19/", "../secret");
        assert!(out.starts_with("/api/cameras/sw-19/stream/"));
    }

    #[test]
    fn polrewrites_master_child_refs() {
        let input = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-STREAM-INF:BANDWIDTH=2000000\n/stream/cam_1/channel/0/hls/abc/playlist.m3u8?ticket=v1%7Ccam_1%7C1%7C1794384651%7Csecret\n";
        let out = rewrite_pol_playlist(input, "resta-partner-4");

        assert!(out.contains("/api/cameras/resta-partner-4/stream/playlist.m3u8"));
        // The ticket must never reach the client.
        assert!(!out.contains("ticket="));
        assert!(!out.contains("1794384651"));
        assert!(out.contains("#EXT-X-STREAM-INF"));
    }

    #[test]
    fn polrewrites_media_segment_refs() {
        let input = "#EXTM3U\n#EXT-X-MAP:URI=\"/stream/cam_1/channel/0/hls/abc/init.mp4?ticket=t\"\n#EXTINF:4.039,\n/stream/cam_1/channel/0/hls/abc/seg-0.m4s?ticket=t\n";
        let out = rewrite_pol_playlist(input, "resta-partner-4");

        assert!(out.contains("URI=\"/api/cameras/resta-partner-4/stream/init.mp4\""));
        assert!(out.contains("/api/cameras/resta-partner-4/stream/seg-0.m4s"));
        assert!(!out.contains("ticket="));
    }

    #[test]
    fn polkeeps_playlist_directives_intact() {
        let input = "#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXT-X-MEDIA-SEQUENCE:0\n";
        let out = rewrite_pol_playlist(input, "resta-partner-4");
        assert!(out.contains("#EXT-X-TARGETDURATION:5"));
        assert!(out.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    }

    #[test]
    fn polsession_splits_base_and_ticket() {
        let master = "https://bdl01.rtsp.itn.net.id/stream/cam_58/channel/0/hls/29f6d6/master.m3u8?ticket=v1%7Ccam_58%7C1%7C1794%7C27.1.1.1.deadbeef";
        let s = session_from_master(master);

        assert_eq!(s.base, "https://bdl01.rtsp.itn.net.id/stream/cam_58/channel/0/hls/29f6d6/");
        assert_eq!(s.ticket, "?ticket=v1%7Ccam_58%7C1%7C1794%7C27.1.1.1.deadbeef");
    }

    #[test]
    fn polhandles_master_without_ticket() {
        // Cryptic but valid: some gateways serve unsigned manifests.
        let s = session_from_master("https://bdl01.rtsp.itn.net.id/hls/x/master.m3u8");
        assert_eq!(s.base, "https://bdl01.rtsp.itn.net.id/hls/x/");
        assert_eq!(s.ticket, "");
    }

    #[test]
    fn polasset_rejects_encoded_traversal() {
        let out = pol_asset_path("resta-1", "/a/b/../../../etc/passwd?ticket=x");
        assert!(out.starts_with("/api/cameras/resta-1/stream/"));
        assert!(!out.contains(".."));
    }

    #[test]
    fn upstream_status_keeps_auth_rejections_distinct() {
        // The Polresta session logic clears a cached session on 401/403. If these
        // were flattened into 502 that branch would be unreachable and a revoked
        // ticket would be trusted until its TTL expired.
        use reqwest::StatusCode as Up;
        assert_eq!(upstream_status(Up::FORBIDDEN), StatusCode::FORBIDDEN);
        assert_eq!(upstream_status(Up::UNAUTHORIZED), StatusCode::UNAUTHORIZED);
        assert_eq!(
            upstream_status(Up::SERVICE_UNAVAILABLE),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(upstream_status(Up::NOT_FOUND), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn pol_auth_rejection_matches_only_401_and_403() {
        assert!(is_pol_auth_rejection(StatusCode::UNAUTHORIZED));
        assert!(is_pol_auth_rejection(StatusCode::FORBIDDEN));
        assert!(!is_pol_auth_rejection(StatusCode::BAD_GATEWAY));
        assert!(!is_pol_auth_rejection(StatusCode::NOT_FOUND));
    }

    #[test]
    fn pol_session_ttl_outlives_no_ticket() {
        // The gateway signs a ticket with a fixed 60 s expiry, so a cache window
        // at or above that guarantees the tail of every window serves 403s.
        assert!(
            POL_SESSION_TTL < std::time::Duration::from_secs(60),
            "POL_SESSION_TTL must stay under the gateway's 60 s ticket expiry"
        );
    }

    #[test]
    fn pol_asset_accepts_real_playlist_artifacts() {
        assert!(is_pol_asset("init.mp4"));
        assert!(is_pol_asset("seg-0.m4s"));
        assert!(is_pol_asset("playlist.m3u8"));
        assert!(is_pol_asset("master.m3u8"));
    }

    #[test]
    fn pol_asset_refuses_anything_else() {
        assert!(!is_pol_asset(""));
        assert!(!is_pol_asset("../../etc/passwd"));
        assert!(!is_pol_asset("a/b/seg-0.m4s"));
        assert!(!is_pol_asset("seg-0.m4s/../../x"));
        assert!(!is_pol_asset("index.html"));
    }

    #[test]
    fn sw_upstream_status_maps_cleanly() {
        use reqwest::StatusCode as Up;
        assert_eq!(sw_upstream_status(Up::NOT_FOUND), StatusCode::NOT_FOUND);
        assert_eq!(sw_upstream_status(Up::INTERNAL_SERVER_ERROR), StatusCode::BAD_GATEWAY);
        assert_eq!(sw_upstream_status(Up::UNAUTHORIZED), StatusCode::BAD_GATEWAY);
        assert_eq!(sw_upstream_status(Up::SERVICE_UNAVAILABLE), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn sw_transient_covers_reauth_and_warmup_but_not_missing_paths() {
        use reqwest::StatusCode as Up;
        // 401: source briefly re-authenticating. 500: muxer not attached yet.
        assert!(is_sw_transient(Up::UNAUTHORIZED));
        assert!(is_sw_transient(Up::FORBIDDEN));
        assert!(is_sw_transient(Up::INTERNAL_SERVER_ERROR));
        assert!(is_sw_transient(Up::SERVICE_UNAVAILABLE));
        // 404 is a real answer: that path or segment is genuinely gone.
        assert!(!is_sw_transient(Up::NOT_FOUND));
        assert!(!is_sw_transient(Up::OK));
    }
}
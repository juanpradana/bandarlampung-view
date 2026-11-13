//! HTTP routes.

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use cctv_web_core::{CameraPublic, Tag};

use crate::proxy;
use crate::state::AppState;

async fn block_data_files() -> StatusCode {
    StatusCode::NOT_FOUND
}

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/cameras", get(list_cameras))
        .route("/api/cameras/:id", get(get_camera))
        .route("/api/cameras/:id/stream", get(proxy::stream_entry))
        .route("/api/cameras/:id/stream/*asset", get(proxy::stream_asset))
        .route("/api/cameras/:id/snapshot", get(proxy::snapshot))
        .route("/api/cameras/:id/thumb", get(proxy::thumb))
        .route("/api/cameras/:id/embed", get(proxy::embed))
        .route("/api/cameras/:id/resolve", get(proxy::resolve_hls))
        .route("/api/stats", get(stats))
        .route("/api/presets", get(presets))
        .route("/api/cameras.geojson", get(cameras_geojson))
        .route("/data/*rest", get(block_data_files))
        .layer(axum::middleware::from_fn_with_state(
            state,
            version_etag,
        ))
}

/// Adds a weak ETag derived from the dataset generation time so clients can
/// revalidate cheaply, and answers `If-None-Match` with 304.
async fn version_etag(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let etag = format!("\"{}\"", state.dataset().generated_at);

    if let Some(inm) = req.headers().get(header::IF_NONE_MATCH) {
        if inm.to_str().ok() == Some(etag.as_str()) {
            return StatusCode::NOT_MODIFIED.into_response();
        }
    }

    let mut resp = next.run(req).await;
    if let Ok(v) = HeaderValue::from_str(&etag) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    resp
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    /// Filter by tag: macet | strategis | ptz
    tag: Option<String>,
    /// Filter by source: seribuwajah | polresta
    source: Option<String>,
    /// Case-insensitive substring match on name
    q: Option<String>,
    /// Only cameras that can show a live picture
    live: Option<bool>,
}

async fn list_cameras(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> impl IntoResponse {
    let dataset = state.dataset();

    let want_tag = q.tag.as_deref().and_then(parse_tag);
    let want_source = q.source.as_deref().map(|s| s.to_lowercase());
    let needle = q.q.as_deref().map(|s| s.to_lowercase());

    let cameras: Vec<CameraPublic> = dataset
        .cameras
        .iter()
        .filter(|c| match want_tag {
            Some(t) => c.tags.contains(&t),
            None => true,
        })
        .filter(|c| match &want_source {
            Some(s) => source_str(c.source) == *s,
            None => true,
        })
        .filter(|c| match &needle {
            Some(n) => c.name.to_lowercase().contains(n),
            None => true,
        })
        .filter(|c| match q.live {
            Some(true) => c.has_live(),
            _ => true,
        })
        .map(CameraPublic::from)
        .collect();

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );

    let body = Json(serde_json::json!({
        "generated_at": dataset.generated_at,
        "total": cameras.len(),
        "cameras": cameras,
    }));

    (headers, body)
}

fn source_str(s: cctv_web_core::Source) -> String {
    match s {
        cctv_web_core::Source::SeribuWajah => "seribuwajah".into(),
        cctv_web_core::Source::Polresta => "polresta".into(),
    }
}

fn parse_tag(s: &str) -> Option<Tag> {
    match s.to_lowercase().as_str() {
        "macet" => Some(Tag::Macet),
        "strategis" => Some(Tag::Strategis),
        "ptz" => Some(Tag::Ptz),
        _ => None,
    }
}

async fn get_camera(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    match state.dataset().find(&id) {
        Some(cam) => Json(CameraPublic::from(cam)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn stats(State(state): State<AppState>) -> impl IntoResponse {
    let d = state.dataset();
    Json(serde_json::json!({
        "generated_at": d.generated_at,
        "total": d.cameras.len(),
        "by_tag": {
            "macet": d.count_by_tag(Tag::Macet),
            "strategis": d.count_by_tag(Tag::Strategis),
            "ptz": d.count_by_tag(Tag::Ptz),
        },
        "by_source": {
            "seribuwajah": d.cameras.iter().filter(|c| c.source == cctv_web_core::Source::SeribuWajah).count(),
            "polresta": d.cameras.iter().filter(|c| c.source == cctv_web_core::Source::Polresta).count(),
        },
        "online": d.cameras.iter().filter(|c| c.has_live()).count(),
    }))
}

async fn presets(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({ "presets": state.dataset().presets }))
}

async fn cameras_geojson(State(state): State<AppState>) -> impl IntoResponse {
    let d = state.dataset();
    let features: Vec<serde_json::Value> = d
        .cameras
        .iter()
        .map(|cam| {
            serde_json::json!({
                "type": "Feature",
                "geometry": {
                    "type": "Point",
                    "coordinates": [cam.longitude, cam.latitude]
                },
                "properties": {
                    "id": cam.id,
                    "source": cam.source,
                    "name": cam.name,
                    "category": cam.category,
                    "district": cam.district,
                    "subdistrict": cam.subdistrict,
                    "status": cam.status,
                    "playback": cam.playback,
                    "tags": cam.tags.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
                    "hasThumbnail": cam.thumbnail.is_some(),
                }
            })
        })
        .collect();

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/geo+json"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );

    let fc = serde_json::json!({
        "type": "FeatureCollection",
        "generated_at": d.generated_at,
        "features": features,
        "presets": d.presets,
    });

    (headers, Json(fc))
}
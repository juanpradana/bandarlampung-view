//! Upstream stream-liveness probe.
//!
//! The Seribu Wajah API reports `status: "online"` for every camera whose RTSP
//! source is running, but a camera's *muxer* — the process that produces the
//! HLS playlist — fails independently and is not reflected in that field. A
//! dead muxer answers `500 {"error":"muxer instance not available"}`, or `404`
//! when the stream path was never created. In the field that is 16 of 263
//! cameras, and picking one of them into a preset yields a black tile.
//!
//! So we ask the origin directly, once per pipeline run, and cache the result.
//! Offline runs reuse the cache, which keeps `--offline` fully deterministic.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use cctv_web_core::{Camera, CameraStatus};

/// Origin serving the Seribu Wajah HLS tree.
const SW_STREAM_HOST: &str = "https://seribuwajah.bandarlampungkota.go.id";
/// The hotlink guard requires both headers to look like the map page itself.
const SW_REFERER: &str = "https://seribuwajah.bandarlampungkota.go.id/map";
const SW_ORIGIN: &str = "https://seribuwajah.bandarlampungkota.go.id";

/// How many probes run at once. The origin is behind Cloudflare and a single
/// client opening hundreds of sockets gets rate-limited (error 1010).
const PROBE_CONCURRENCY: usize = 8;

/// Cached probe result, written next to the raw source dumps.
#[derive(Debug, Serialize, Deserialize)]
pub struct LivenessCache {
    /// Cameras whose stream answered with a playlist at probe time.
    pub live: Vec<String>,
    /// Cameras whose muxer was down (500/404/timeout).
    pub down: Vec<String>,
}

/// Probe every Seribu Wajah camera's playlist and report which are reachable.
///
/// Only Seribu Wajah is probed: Polresta cameras are iframes served by a third
/// party, and the only cheap check there is the `stream_status` field the map
/// API already reports.
pub async fn probe_seribuwajah(cameras: &[Camera]) -> Result<LivenessCache> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .user_agent("cctv-dashboard/0.1 (+https://bandarlampungkota.go.id)")
        .build()?;

    let targets: Vec<(&str, &str)> = cameras
        .iter()
        .filter(|c| c.source == cctv_web_core::Source::SeribuWajah)
        .map(|c| (c.id.as_str(), c.source_key.as_str()))
        .collect();

    let total = targets.len();
    let mut live = Vec::new();
    let mut down = Vec::new();

    // Chunk so we never hold `PROBE_CONCURRENCY` futures more than intended.
    for chunk in targets.chunks(PROBE_CONCURRENCY) {
        let mut set = Vec::with_capacity(chunk.len());
        for (id, key) in chunk {
            let client = client.clone();
            let id = id.to_string();
            let url = format!("{SW_STREAM_HOST}/cctv_{key}/index.m3u8");
            set.push(tokio::spawn(async move {
                let res = client
                    .get(&url)
                    .header(reqwest::header::REFERER, SW_REFERER)
                    .header(reqwest::header::ORIGIN, SW_ORIGIN)
                    .header(reqwest::header::ACCEPT, "*/*")
                    .send()
                    .await;

                let ok = match res {
                    Ok(r) => r.status().is_success(),
                    Err(_) => false,
                };
                (id, ok)
            }));
        }

        for handle in set {
            match handle.await {
                Ok((id, true)) => live.push(id),
                Ok((id, false)) => down.push(id),
                Err(e) => warn!(error = %e, "probe task panicked"),
            }
        }
    }

    live.sort();
    down.sort();
    info!(
        total,
        live = live.len(),
        down = down.len(),
        "stream liveness probe complete"
    );

    Ok(LivenessCache { live, down })
}

/// Mark cameras whose probe failed as offline so presets and `live` filters
/// stop selecting them. A camera that answers is left exactly as reported.
pub fn apply(cameras: &mut [Camera], cache: &LivenessCache) {
    let down: HashSet<&str> = cache.down.iter().map(String::as_str).collect();

    for cam in cameras.iter_mut() {
        if down.contains(cam.id.as_str()) {
            cam.status = CameraStatus::Offline;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cctv_web_core::Playback;

    fn cam(id: &str) -> Camera {
        Camera {
            id: id.into(),
            source: cctv_web_core::Source::SeribuWajah,
            source_key: id.trim_start_matches("sw-").into(),
            name: id.into(),
            city_address: None,
            district: None,
            subdistrict: None,
            category: None,
            latitude: -5.4,
            longitude: 105.2,
            playback: Playback::Hls,
            stream_url: Some("https://example.com/live.m3u8".into()),
            embed_url: None,
            thumbnail: None,
            token: None,
            status: CameraStatus::Online,
            tags: vec![],
        }
    }

    #[test]
    fn apply_marks_probed_dead_cameras_offline() {
        let mut cams = vec![cam("sw-19"), cam("sw-81"), cam("sw-322")];
        let cache = LivenessCache {
            live: vec!["sw-19".into()],
            down: vec!["sw-81".into(), "sw-322".into()],
        };

        apply(&mut cams, &cache);

        assert_eq!(cams[0].status, CameraStatus::Online);
        assert_eq!(cams[1].status, CameraStatus::Offline);
        assert_eq!(cams[2].status, CameraStatus::Offline);
    }

    #[test]
    fn apply_leaves_cameras_absent_from_both_lists_alone() {
        // A probe that never ran (empty cache) must not silently take every
        // camera offline — that would empty the whole dashboard.
        let mut cams = vec![cam("sw-19")];
        apply(&mut cams, &LivenessCache { live: vec![], down: vec![] });
        assert_eq!(cams[0].status, CameraStatus::Online);
    }
}
//! Application state: the loaded dataset + a shared HTTP client.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use reqwest::Client;

use cctv_web_core::Dataset;

/// A resolved Polresta HLS session.
///
/// The ITN gateway mints a short-lived signed URL whose ticket is bound to the
/// requesting IP, so it can only be used by this server. We keep it in memory
/// and hand the browser opaque `/stream/pol/...` paths instead, which keeps the
/// ticket off the wire entirely.
#[derive(Debug, Clone)]
pub struct PolSession {
    /// Directory URL on the gateway that all playlist/segment paths hang off,
    /// e.g. `https://bdl01.rtsp.itn.net.id/stream/{cam}/channel/0/hls/{sid}/`.
    pub base: String,
    /// Query string (with the leading `?`) carrying the ticket.
    pub ticket: String,
    pub resolved_at: Instant,
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    dataset: Dataset,
    http: Client,
    /// Resolved Polresta sessions, keyed by camera id.
    pol_sessions: Mutex<HashMap<String, PolSession>>,
}

impl AppState {
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("cameras.json");
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let dataset: Dataset = serde_json::from_str(&raw).context("parse cameras.json")?;

        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .user_agent("cctv-dashboard/0.1 (+https://bandarlampungkota.go.id)")
            .build()?;

        Ok(Self {
            inner: Arc::new(Inner {
                dataset,
                http,
                pol_sessions: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn dataset(&self) -> &Dataset {
        &self.inner.dataset
    }

    pub fn http(&self) -> &Client {
        &self.inner.http
    }

    pub fn pol_session(&self, camera_id: &str) -> Option<PolSession> {
        self.inner.pol_sessions.lock().ok()?.get(camera_id).cloned()
    }

    pub fn set_pol_session(&self, camera_id: &str, session: PolSession) {
        if let Ok(mut map) = self.inner.pol_sessions.lock() {
            map.insert(camera_id.to_string(), session);
        }
    }

    pub fn clear_pol_session(&self, camera_id: &str) {
        if let Ok(mut map) = self.inner.pol_sessions.lock() {
            map.remove(camera_id);
        }
    }
}
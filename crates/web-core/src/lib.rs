//! Shared domain model: unified camera records + classification rules.
//!
//! Used by the CLI (data pipeline), the API (serialization for the client),
//! and the WASM client. No I/O here — pure data + rules so it compiles for both
//! native and `wasm32-unknown-unknown`.

use serde::{Deserialize, Serialize};

pub mod classify;

pub use classify::{classify, deduplicate};

/// Which upstream website this record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Diskominfo "Seribu Wajah" — 264 cameras, HLS behind a referer-gated proxy.
    SeribuWajah,
    /// Polresta Bandar Lampung — 119 cameras, iframe embeds from rtsp.itn.net.id.
    Polresta,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::SeribuWajah => "Seribu Wajah",
            Source::Polresta => "Polresta",
        }
    }
}

/// Classification tags. A camera can carry several (e.g. a PTZ on a protocol
/// road is both `Macet` and `Ptz`, and may also be `Strategis`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tag {
    /// Traffic monitoring — roads, intersections, flyovers.
    #[serde(rename = "macet")]
    Macet,
    /// Strategic location — government, health, market, transit.
    #[serde(rename = "strategis")]
    Strategis,
    /// Pan/tilt/zoom camera.
    #[serde(rename = "ptz")]
    Ptz,
}

impl Tag {
    pub fn as_str(self) -> &'static str {
        match self {
            Tag::Macet => "macet",
            Tag::Strategis => "strategis",
            Tag::Ptz => "ptz",
        }
    }
}

/// How a client should render this stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Playback {
    /// HLS destined for an `<video>` element via our strip proxy.
    Hls,
    /// Load the provider's own player in an `<iframe>`.
    Embed,
    /// A still image only — no live stream attached.
    SnapshotOnly,
}

/// A single upstream camera, normalized across both sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Camera {
    /// Stable id, namespaced by source: `sw-{id}` / `resta-{slug}`.
    pub id: String,
    pub source: Source,
    /// Native upstream id (Seribu Wajah numeric id, Polresta slug).
    pub source_key: String,
    pub name: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub city_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub district: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdistrict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,

    /// WGS84 decimal degrees.
    pub latitude: f64,
    pub longitude: f64,

    pub playback: Playback,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embed_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    pub status: CameraStatus,
    pub tags: Vec<Tag>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CameraStatus {
    Online,
    Offline,
}

impl Camera {
    /// Path segment for the stream strip on our own origin.
    pub fn strip_key(&self) -> String {
        self.id.clone()
    }

    /// Whether this record can show a live picture at all.
    pub fn has_live(&self) -> bool {
        self.status == CameraStatus::Online
            && (self.stream_url.is_some() || self.embed_url.is_some())
    }
}

/// A group of cameras clustered into one multi-stream preset tile.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraPreset {
    /// `traffic-corridor` | `intersection` | `strategic-cluster` | `custom`.
    pub id: String,
    pub label: String,
    pub camera_ids: Vec<String>,
    /// CSS `grid-template-areas` string used by the client.
    pub grid_template: String,
    pub cols: u32,
    pub rows: u32,
}

/// The merged dataset the API serves to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dataset {
    pub generated_at: String,
    pub cameras: Vec<Camera>,
    pub presets: Vec<CameraPreset>,
}

impl Dataset {
    pub fn find(&self, id: &str) -> Option<&Camera> {
        self.cameras.iter().find(|c| c.id == id)
    }

    pub fn count_by_tag(&self, tag: Tag) -> usize {
        self.cameras.iter().filter(|c| c.tags.contains(&tag)).count()
    }
}
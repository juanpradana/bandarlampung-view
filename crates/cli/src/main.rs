//! CLI data pipeline: fetch both upstream sources, deduplicate, classify, emit GeoJSON.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use cctv_web_core::{
    Camera, CameraPreset, CameraStatus, Dataset, Playback, Source, Tag, classify, deduplicate,
};

mod liveness;

/// Seribu Wajah raw camera schema (from `/cctvs`).
///
/// The upstream payload is camelCase; `rename_all` maps it onto snake_case here.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwCamera {
    id: i64,
    name: String,
    category: String,
    latitude: f64,
    longitude: f64,
    address: Option<String>,
    thumbnail: Option<String>,
    is_hidden: bool,
    status: String,
    kelurahan: Option<SwKelurahan>,
    stream_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwKelurahan {
    name: String,
    kecamatan: Option<SwKecamatan>,
}

#[derive(Debug, Deserialize)]
struct SwKecamatan {
    name: String,
}

/// Polresta `map-points` FeatureCollection item.
#[derive(Debug, Deserialize)]
struct PolrestaFeature {
    geometry: PolrestaGeometry,
    properties: PolrestaProperties,
}

#[derive(Debug, Deserialize)]
struct PolrestaGeometry {
    /// `[longitude, latitude]`
    coordinates: [f64; 2],
}

#[derive(Debug, Deserialize)]
struct PolrestaProperties {
    name: String,
    /// `cctv` | `polisi` | `pelayanan` | `pospol`
    #[serde(rename = "type")]
    #[allow(dead_code)]
    p_type: String,
    layer_label: String,
    /// Absent on a handful of entries — use `token` as `Option`.
    subtitle: Option<String>,
    embed_url: String,
    stream_status: String,
    thumb_small: String,
    slug: String,
}

#[derive(Parser, Debug)]
#[command(version, about = "Bandar Lampung CCTV data pipeline")]
struct Args {
    /// Output directory for generated JSON files
    #[arg(short, long, default_value = "static/data")]
    output_dir: PathBuf,

    /// Reuse cached raw JSON instead of hitting the upstream APIs
    #[arg(long)]
    offline: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    fs::create_dir_all(&args.output_dir).context("create output dir")?;

    // ---- 1. FETCH (or load cache) -------------------------------------
    let (sw_json, resta_json) = if args.offline {
        info!("offline mode — reading cached raw JSON");
        (
            read_json(&args.output_dir.join("seribuwajah_raw.json"))?,
            read_json(&args.output_dir.join("polresta_raw.json"))?,
        )
    } else {
        info!("fetching upstream sources");
        let sw = get_json("https://api-newseribuwajah.bandarlampungkota.go.id/cctvs", true).await?;
        let resta = get_json(
            "https://restabandarlampung.lampung.polri.go.id/api/map-points",
            false,
        )
        .await?;

        write_json(&args.output_dir.join("seribuwajah_raw.json"), &sw)?;
        write_json(&args.output_dir.join("polresta_raw.json"), &resta)?;
        (sw, resta)
    };

    // ---- 2. NORMALIZE -------------------------------------------------
    let mut cameras = Vec::new();
    cameras.extend(normalize_seribuwajah(&sw_json)?);
    cameras.extend(normalize_polresta(&resta_json)?);
    info!("normalized {} cameras", cameras.len());

    // ---- 2b. LIVENESS -------------------------------------------------
    // The Seribu Wajah API's `status` tracks the RTSP source, not the muxer
    // that actually serves HLS, so a handful of cameras claim "online" and
    // answer 500. Probe the origin and cache the result; offline runs reuse it.
    let liveness_path = args.output_dir.join("liveness.json");
    let liveness = if args.offline {
        match read_json(&liveness_path) {
            Ok(v) => serde_json::from_value(v).unwrap_or_else(|e| {
                warn!(error = %e, "liveness cache unreadable; skipping probe");
                liveness::LivenessCache { live: vec![], down: vec![] }
            }),
            Err(_) => {
                warn!("no liveness cache; every reported-online camera stays online");
                liveness::LivenessCache { live: vec![], down: vec![] }
            }
        }
    } else {
        let res = liveness::probe_seribuwajah(&cameras).await?;
        write_json(&liveness_path, &res)?;
        res
    };
    liveness::apply(&mut cameras, &liveness);

    // ---- 3. CLASSIFY --------------------------------------------------
    for cam in &mut cameras {
        cam.tags = classify(cam);
    }
    let count = |t: Tag| cameras.iter().filter(|c| c.tags.contains(&t)).count();
    info!(
        macet = count(Tag::Macet),
        strategis = count(Tag::Strategis),
        ptz = count(Tag::Ptz),
        "classification complete"
    );

    // ---- 4. DEDUPLICATE -----------------------------------------------
    let before = cameras.len();
    cameras = deduplicate(cameras);
    info!(
        before,
        after = cameras.len(),
        merged = before - cameras.len(),
        "deduplication complete"
    );

    // ---- 5. PRESETS ---------------------------------------------------
    let presets = build_presets(&cameras);

    // ---- 6. EMIT ------------------------------------------------------
    let dataset = Dataset {
        generated_at: chrono::Utc::now().to_rfc3339(),
        cameras,
        presets,
    };

    let json_path = args.output_dir.join("cameras.json");
    write_json(&json_path, &dataset)?;

    let geo_path = args.output_dir.join("cameras.geojson");
    write_geojson(&geo_path, &dataset)?;

    info!(path = %json_path.display(), "wrote dataset");

    if dataset.cameras.is_empty() {
        warn!("dataset is empty — check upstream availability");
    }

    Ok(())
}

// ---------------------------------------------------------------- fetch

async fn get_json(url: &str, with_api_key: bool) -> Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()?;

    let mut req = client.get(url).header("accept", "application/json");
    if with_api_key {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static(SW_API_KEY));
        req = req.headers(headers);
    }

    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("{url} returned {status}");
    }

    resp.json().await.with_context(|| format!("parse JSON from {url}"))
}

/// Diskominfo's public web client ships this key in its bundle.
const SW_API_KEY: &str = "ParkirIlegalDetectKey2026";

// --------------------------------------------------------- normalize

fn normalize_seribuwajah(root: &Value) -> Result<Vec<Camera>> {
    let raw: Vec<SwCamera> = serde_json::from_value(root.clone())
        .context("parse Seribu Wajah payload")?;

    let mut out = Vec::with_capacity(raw.len());
    for c in raw {
        if c.is_hidden {
            continue;
        }

        let (district, subdistrict) = match c.kelurahan {
            Some(k) => {
                let kec = k.kecamatan.map(|kec| kec.name);
                (kec, Some(k.name))
            }
            None => (None, None),
        };

        out.push(Camera {
            id: format!("sw-{}", c.id),
            source: Source::SeribuWajah,
            source_key: c.id.to_string(),
            name: c.name,
            city_address: c.address,
            district,
            subdistrict,
            category: Some(c.category),
            latitude: c.latitude,
            longitude: c.longitude,
            playback: Playback::Hls,
            stream_url: c.stream_url,
            embed_url: None,
            thumbnail: c.thumbnail,
            token: None,
            status: parse_status(&c.status),
            tags: Vec::new(),
        });
    }
    Ok(out)
}

fn normalize_polresta(root: &Value) -> Result<Vec<Camera>> {
    // Only `type == "cctv"` entries carry the video fields (embed_url, stream_status,
    // thumbnails). The other layers (polisi/pelayanan/pospol) are map pins with a
    // different shape, so filter on the raw value before deserializing.
    let features = root
        .get("features")
        .and_then(Value::as_array)
        .context("Polresta payload has no features array")?;

    let mut out = Vec::new();
    for feature in features {
        if feature.pointer("/properties/type").and_then(Value::as_str) != Some("cctv") {
            continue;
        }

        let f: PolrestaFeature = serde_json::from_value(feature.clone()).with_context(|| {
            format!(
                "parse Polresta feature {}",
                feature
                    .pointer("/properties/name")
                    .and_then(Value::as_str)
                    .unwrap_or("<unnamed>")
            )
        })?;

        out.push(Camera {
            id: format!("resta-{}", f.properties.slug),
            source: Source::Polresta,
            source_key: f.properties.slug.clone(),
            name: f.properties.name,
            city_address: None,
            district: None,
            subdistrict: None,
            category: Some(f.properties.layer_label),
            latitude: f.geometry.coordinates[1],
            longitude: f.geometry.coordinates[0],
            playback: Playback::Embed,
            stream_url: None,
            embed_url: Some(f.properties.embed_url),
            thumbnail: Some(f.properties.thumb_small),
            token: f.properties.subtitle,
            status: if f.properties.stream_status == "ready" {
                CameraStatus::Online
            } else {
                CameraStatus::Offline
            },
            tags: Vec::new(),
        });
    }
    Ok(out)
}

fn parse_status(s: &str) -> CameraStatus {
    match s.to_lowercase().as_str() {
        "online" | "aktif" | "ready" => CameraStatus::Online,
        _ => CameraStatus::Offline,
    }
}

// ----------------------------------------------------------- presets

/// High-priority civic & strategic landmarks for the strategic-cluster preset.
const CIVIC_KEYWORDS: &[&str] = &[
    "HALAMAN PEMKOT",
    "PINTU MASUK PEMKOT",
    "PINTU KELUAR PEMKOT",
    "GEDUNG DPRD",
    "DPRD KOTA",
    "MASJID AL-BAKRIE",
    "MASJID TAQWA",
    "RS ABDUL MOELOEK",
    "RS IMMANUEL",
    "TERMINAL RAJABASA",
    "TERMINAL KEMILING",
    "PASAR TUGU",
    "PASAR GINTUNG",
    "PASAR PANJANG",
    "MALL KARTINI",
    "MALL BOEMI KEDATON",
    "GERBANG UNILA",
    "PKOR WAY HALIM",
    "HOTEL NOVOTEL",
];

/// Major traffic corridor / intersection landmarks for traffic presets.
const TRAFFIC_KEYWORDS: &[&str] = &[
    "JEMBATAN MILLENIAL",
    "UNDERPASS UNILA",
    "SIMPANG LUNGSIR",
    "SIMPANG KOTA KARANG",
    "SIMPANG PALANG BESI",
    "FLYOVER KIMAJA",
    "FLY OVER PRAMUKA",
    "FLYOVER UNTUNG",
    "BAWAH FLY OVER",
    "PEREMPATAN PU",
    "TUGU ADIPURA",
    "JL. SUDIRMAN",
    "JL. GAJAH MADA",
    "JL. RADEN INTAN",
];

fn matches_any(name: &str, keywords: &[&str]) -> bool {
    let upper = name.to_uppercase();
    keywords.iter().any(|k| upper.contains(k))
}

/// Extract the first token of a camera name, used as a coarse "site" key.
///
/// `PASAR GINTUNG 1` and `PASAR GINTUNG 2` are two angles on one place, and so
/// are the three `HALAMAN PEMKOT` / `PINTU ... PEMKOT` cameras. Without this,
/// a keyword pass fills a preset with several views of a single building.
fn site_key(name: &str) -> String {
    name.split_whitespace()
        .next()
        .unwrap_or("")
        .to_uppercase()
}

/// Pick up to `n` cameras favouring one per site and alternating sources.
///
/// Walks the input in order; a camera is skipped when its site or its source
/// has already been used as many times as it can afford. Falls back to filling
/// remaining slots in input order so a small pool still yields a full preset.
fn pick_diverse(cands: &[&Camera], n: usize) -> Vec<String> {
    let mut picked: Vec<String> = Vec::new();
    let mut sites: Vec<String> = Vec::new();
    let mut sw_count = 0usize;
    let mut resta_count = 0usize;

    // Half the tiles from each source when both are available.
    let per_source_cap = n.div_ceil(2);

    for c in cands {
        if picked.len() >= n {
            break;
        }
        let site = site_key(&c.name);
        if sites.contains(&site) {
            continue;
        }
        match c.source {
            Source::SeribuWajah if sw_count >= per_source_cap => continue,
            Source::Polresta if resta_count >= per_source_cap => continue,
            _ => {}
        }

        picked.push(c.id.clone());
        sites.push(site);
        match c.source {
            Source::SeribuWajah => sw_count += 1,
            Source::Polresta => resta_count += 1,
        }
    }

    // Top up ignoring the diversity constraints if the pool was too thin.
    for c in cands {
        if picked.len() >= n {
            break;
        }
        if !picked.contains(&c.id) {
            picked.push(c.id.clone());
        }
    }

    picked
}

/// Curated multi-stream layouts.
///
/// Curated by landmark significance and source diversity rather than taking
/// the first N rows of an arbitrarily ordered list (which previously bundled
/// random village offices or duplicated the corridor across the intersection).
fn build_presets(cameras: &[Camera]) -> Vec<CameraPreset> {
    let live: Vec<&Camera> = cameras.iter().filter(|c| c.has_live()).collect();

    // 1. Traffic corridor: 4 major arterial / flyover feeds.
    // Interleave sources so the user sees both Diskominfo and Polresta coverage.
    let mut corridor: Vec<String> = Vec::new();
    let corridor_sw = live
        .iter()
        .filter(|c| c.source == Source::SeribuWajah && c.tags.contains(&Tag::Macet))
        .filter(|c| matches_any(&c.name, TRAFFIC_KEYWORDS));
    let corridor_resta = live
        .iter()
        .filter(|c| c.source == Source::Polresta && c.tags.contains(&Tag::Macet))
        .filter(|c| matches_any(&c.name, TRAFFIC_KEYWORDS));

    for (a, b) in corridor_sw.zip(corridor_resta) {
        if corridor.len() < 4 && !corridor.contains(&a.id) {
            corridor.push(a.id.clone());
        }
        if corridor.len() < 4 && !corridor.contains(&b.id) {
            corridor.push(b.id.clone());
        }
        if corridor.len() >= 4 {
            break;
        }
    }
    // Top-up if keywords didn't yield 4:
    for c in live.iter().filter(|c| c.tags.contains(&Tag::Macet)) {
        if corridor.len() >= 4 {
            break;
        }
        if !corridor.contains(&c.id) {
            corridor.push(c.id.clone());
        }
    }

    // 2. Intersection (1 main PTZ + 3 approach views):
    // Main tile: an active PTZ overview.
    // Approaches: 3 traffic cameras DISTINCT from the corridor.
    let mut intersection: Vec<String> = Vec::new();
    if let Some(ptz) = live.iter().find(|c| c.tags.contains(&Tag::Ptz)) {
        intersection.push(ptz.id.clone());
    }
    for c in live.iter().filter(|c| c.tags.contains(&Tag::Macet)) {
        if intersection.len() >= 4 {
            break;
        }
        if !corridor.contains(&c.id) && !intersection.contains(&c.id) {
            intersection.push(c.id.clone());
        }
    }

    // 3. Strategic cluster (3x2, 6 civic/health/transit/market cameras):
    // Curated with high-profile civic locations across both sources, explicitly
    // skipping cameras known to be down upstream (e.g. sw-81).
    //
    // Site-deduplicated (one camera per place, not three angles of the same
    // gate) and source-alternated, so the six tiles read as six different
    // places rather than a slideshow of one building.
    let strategic_civic = live
        .iter()
        .filter(|c| c.tags.contains(&Tag::Strategis))
        .filter(|c| matches_any(&c.name, CIVIC_KEYWORDS))
        .copied()
        .collect::<Vec<_>>();

    let strategic = pick_diverse(&strategic_civic, 6);
    let mut strategic = strategic;
    if strategic.len() < 6 {
        // Landmark matching came up short — top up from the rest, still
        // deduplicated by site and source-interleaved.
        let rest = live
            .iter()
            .filter(|c| c.tags.contains(&Tag::Strategis))
            .filter(|c| !strategic.contains(&c.id))
            .filter(|c| !strategic.contains(&c.id))
            .copied()
            .collect::<Vec<_>>();
        for c in pick_diverse(&rest, 6 - strategic.len()) {
            if !strategic.contains(&c) {
                strategic.push(c);
            }
        }
    }

    vec![
        CameraPreset {
            id: "traffic-corridor".into(),
            label: "Koridor Lalu Lintas".into(),
            camera_ids: corridor,
            grid_template: "\"c1 c2\" \"c3 c4\"".into(),
            cols: 2,
            rows: 2,
        },
        CameraPreset {
            id: "intersection".into(),
            label: "Simpang Utama".into(),
            camera_ids: intersection,
            grid_template: "\"m m m p1\" \"m m m p2\" \"m m m p3\"".into(),
            cols: 4,
            rows: 3,
        },
        CameraPreset {
            id: "strategic-cluster".into(),
            label: "Kluster Strategis".into(),
            camera_ids: strategic,
            grid_template: "\"c1 c2\" \"c3 c4\" \"c5 c6\"".into(),
            cols: 2,
            rows: 3,
        },
    ]
    .into_iter()
    .filter(|p| !p.camera_ids.is_empty())
    .collect()
}

// -------------------------------------------------------------- io

fn read_json(path: &Path) -> Result<Value> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(value)?;
    fs::write(path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn write_geojson(path: &Path, dataset: &Dataset) -> Result<()> {
    use serde_json::json;

    let features: Vec<Value> = dataset
        .cameras
        .iter()
        .map(|cam| {
            json!({
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

    let fc = json!({
        "type": "FeatureCollection",
        "generated_at": dataset.generated_at,
        "features": features,
        "presets": dataset.presets,
    });

    fs::write(path, serde_json::to_string_pretty(&fc)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cctv_web_core::{Playback, Source};

    fn make_cam(id: &str, name: &str, tags: Vec<Tag>) -> Camera {
        Camera {
            id: id.into(),
            source: Source::SeribuWajah,
            source_key: id.into(),
            name: name.into(),
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
            tags,
        }
    }

    #[test]
    fn presets_diversify_corridor_and_intersection() {
        let mut cams = Vec::new();
        // 1 PTZ
        cams.push(make_cam("ptz-1", "HALAMAN PEMKOT (PTZ)", vec![Tag::Ptz, Tag::Macet]));
        // 8 traffic cameras
        for i in 1..=8 {
            cams.push(make_cam(
                &format!("traffic-{i}"),
                &format!("JEMBATAN MILLENIAL {i}"),
                vec![Tag::Macet],
            ));
        }
        // 6 strategic
        for i in 1..=6 {
            cams.push(make_cam(
                &format!("strat-{i}"),
                &format!("KANTOR WALIKOTA {i}"),
                vec![Tag::Strategis],
            ));
        }

        let presets = build_presets(&cams);
        let corridor = presets.iter().find(|p| p.id == "traffic-corridor").unwrap();
        let intersection = presets.iter().find(|p| p.id == "intersection").unwrap();

        assert_eq!(corridor.camera_ids.len(), 4);
        assert_eq!(intersection.camera_ids.len(), 4);

        // Corridor and intersection must NOT be identical:
        assert_ne!(
            corridor.camera_ids, intersection.camera_ids,
            "corridor and intersection presets must not repeat the exact same set"
        );

        // Intersection main tile should be the PTZ:
        assert_eq!(intersection.camera_ids[0], "ptz-1");
    }

    #[test]
    fn matches_any_recognizes_landmarks() {
        assert!(matches_any("HALAMAN PEMKOT (PTZ)", CIVIC_KEYWORDS));
        assert!(matches_any("Jembatan Millenial 1", TRAFFIC_KEYWORDS));
        assert!(!matches_any("KELURAHAN BAKUNG", CIVIC_KEYWORDS));
    }

    #[test]
    fn strategic_cluster_skips_dead_cameras() {
        let mut cams = Vec::new();
        // A dead camera is one whose liveness probe failed — `status` is the
        // only signal the preset builder sees, since classification runs before
        // the probe and the probe overwrites status in place.
        let mut dead = make_cam("sw-81", "HALAMAN PEMKOT", vec![Tag::Strategis]);
        dead.status = CameraStatus::Offline;
        cams.push(dead);
        for i in 1..=6 {
            cams.push(make_cam(
                &format!("sw-{i}"),
                &format!("HALAMAN PEMKOT {i}"),
                vec![Tag::Strategis],
            ));
        }
        let presets = build_presets(&cams);
        let cluster = presets.iter().find(|p| p.id == "strategic-cluster").unwrap();
        assert!(!cluster.camera_ids.contains(&"sw-81".to_string()));
    }
}
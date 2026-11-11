//! Classification & deduplication rules for Bandar Lampung CCTV cameras.
//!
//! Evaluates both textual clues (names, categories, addresses) and geospatial
//! proximity to tag cameras and merge overlapping records.

use strsim::jaro_winkler;
use crate::{Camera, Source, Tag};

/// Earth radius in meters, for Haversine.
const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// Distance threshold (meters) under which two cameras from DIFFERENT sources
/// are checked for name similarity to qualify as a duplicate pair.
pub const DEDUP_RADIUS_M: f64 = 55.0;

/// Minimum Jaro-Winkler similarity on cleaned camera names to declare a match
/// when within `DEDUP_RADIUS_M`.
pub const DEDUP_NAME_SIMILARITY: f64 = 0.58;

/// Compute great-circle distance between two WGS84 points in meters.
pub fn distance_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let delta_phi = (lat2 - lat1).to_radians();
    let delta_lambda = (lon2 - lon1).to_radians();

    let a = (delta_phi / 2.0).sin().powi(2)
        + phi1.cos() * phi2.cos() * (delta_lambda / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());

    EARTH_RADIUS_M * c
}

/// Normalize camera name for keyword matching and similarity checks.
pub fn clean_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let upper = s.to_uppercase();
    for ch in upper.chars() {
        if ch.is_alphanumeric() || ch == ' ' {
            out.push(ch);
        } else {
            out.push(' ');
        }
    }
    // collapse whitespace
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Check if a camera qualifies as "Pantau Macet" (traffic monitoring).
pub fn is_traffic(camera: &Camera) -> bool {
    let name = clean_name(&camera.name);
    let cat = camera.category.as_deref().unwrap_or("").to_uppercase();

    // Seribu Wajah explicit category
    if cat.contains("JALAN PROTOKOL") || cat.contains("JALAN") {
        return true;
    }

    // Traffic keywords common in Bandar Lampung road names and junctions
    const TRAFFIC_KEYWORDS: &[&str] = &[
        "PERTIGAAN", "PEREMPATAN", "SIMPANG", "FLYOVER", "FLY OVER",
        "UNDERPASS", "JEMBATAN", "TUGU", "BUNDARAN", "LAMPU MERAH",
        "TRAFFIC LIGHT", "JL ", "JL.", "JALAN", "ARAH", "LUNGSIR",
        "BEGADANG", "GERBANG", "BYPASS", "BY PASS", "SUDIRMAN",
        "RADEN INTAN", "ZA PAGAR ALAM", "KIMAJA", "ANTASARI",
        "GAJAH MADA", "DIPONEGORO", "YOS SUDARSO", "GATOT SUBROTO",
        "TEUKU UMAR", "KARTINI", "HASANUDIN", "KEDATON", "KEMILING",
    ];

    for kw in TRAFFIC_KEYWORDS {
        if name.contains(kw) {
            return true;
        }
    }

    false
}

/// Check if a camera qualifies as "Pantau Lokasi Strategis" (strategic point).
pub fn is_strategic(camera: &Camera) -> bool {
    let name = clean_name(&camera.name);
    let cat = camera.category.as_deref().unwrap_or("").to_uppercase();

    // Seribu Wajah explicit strategic categories
    if cat.contains("PEMERINTAH")
        || cat.contains("KECAMATAN")
        || cat.contains("KELURAHAN")
        || cat.contains("PUSKES")
        || cat.contains("PASAR")
        || cat.contains("SUNGAI")
    {
        return true;
    }

    // Polresta & civic strategic keywords
    const STRATEGIC_KEYWORDS: &[&str] = &[
        "GEDUNG", "KANTOR", "DPRD", "POLRESTA", "POLSEK", "POSPOL",
        "POS KOTA", "RUMAH SAKIT", "RS ", "RSUD", "PUSKESMAS",
        "PASAR", "MALL", "MBK", "TRANSMART", "STASIUN", "TERMINAL",
        "BANDARA", "PELABUHAN", "DERMAGA", "UNIVERSITAS", "UNILA",
        "UIN", "ITERA", "MASJID", "GEREJA", "KORPRI", "POLDA",
        "PEMKOT", "DINAS", "TPU", "MAKAM", "SABURAI", "PKOR",
        "LAPANGAN", "HOTEL", "NOVOTEL", "EMBUNG",
    ];

    for kw in STRATEGIC_KEYWORDS {
        if name.contains(kw) {
            return true;
        }
    }

    false
}

/// Check if a camera has Pan-Tilt-Zoom capability.
pub fn is_ptz(camera: &Camera) -> bool {
    let raw = &camera.name;
    let upper = raw.to_uppercase();

    // Seribu Wajah explicit "(PTZ)" in name
    if upper.contains("(PTZ)") || upper.contains(" PTZ") || upper.contains("PTZ ") {
        return true;
    }

    // Polresta PTZ entries
    if camera.source_key == "partner-82" // PTZ Masjid AL-Bakrie
        || camera.source_key == "partner-127"
    {
        return true;
    }

    false
}

/// Compute all tags for a camera based on rules.
pub fn classify(camera: &Camera) -> Vec<Tag> {
    let mut tags = Vec::new();

    if is_traffic(camera) {
        tags.push(Tag::Macet);
    }
    if is_strategic(camera) {
        tags.push(Tag::Strategis);
    }
    if is_ptz(camera) {
        tags.push(Tag::Ptz);
    }

    // Fallback: if somehow neither matched, mark as strategis so it shows up in tabs
    if tags.is_empty() {
        tags.push(Tag::Strategis);
    }

    tags
}

/// Merge two cameras that represent the same physical location.
/// Keeps the Seribu Wajah record as base (since it has HLS), but adds
/// Polresta's embed URL and thumbnails for fallback.
pub fn merge_pair(sw: Camera, resta: Camera) -> Camera {
    let mut merged = sw;
    // ensure embed_url is kept from Polresta
    if merged.embed_url.is_none() {
        merged.embed_url = resta.embed_url;
    }
    // keep Polresta thumbnail if Seribu has none
    if merged.thumbnail.is_none() {
        merged.thumbnail = resta.thumbnail;
    }
    // merge tags
    for tag in resta.tags {
        if !merged.tags.contains(&tag) {
            merged.tags.push(tag);
        }
    }
    merged
}

/// Deduplicate a combined list of cameras from both sources.
/// Cameras within `DEDUP_RADIUS_M` with name similarity > `DEDUP_NAME_SIMILARITY`
/// are merged into a single record.
pub fn deduplicate(cameras: Vec<Camera>) -> Vec<Camera> {
    let mut merged_out: Vec<Camera> = Vec::new();
    let mut skip_indices = std::collections::HashSet::new();

    for i in 0..cameras.len() {
        if skip_indices.contains(&i) {
            continue;
        }

        let mut current = cameras[i].clone();

        for (j, candidate) in cameras.iter().enumerate().skip(i + 1) {
            if skip_indices.contains(&j) {
                continue;
            }

            // Only merge across DIFFERENT sources — keep intra-source duplicates distinct
            if current.source == candidate.source {
                continue;
            }

            let dist = distance_meters(
                current.latitude,
                current.longitude,
                candidate.latitude,
                candidate.longitude,
            );

            if dist < DEDUP_RADIUS_M {
                let name_a = clean_name(&current.name);
                let name_b = clean_name(&candidate.name);
                let sim = jaro_winkler(&name_a, &name_b);

                // Either high name similarity or very close distance (< 20m)
                if sim >= DEDUP_NAME_SIMILARITY || dist < 20.0 {
                    // Merge: prefer Seribu Wajah as base if present
                    if current.source == Source::SeribuWajah {
                        current = merge_pair(current, candidate.clone());
                    } else {
                        current = merge_pair(candidate.clone(), current);
                    }
                    skip_indices.insert(j);
                }
            }
        }

        merged_out.push(current);
    }

    merged_out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CameraStatus, Playback};

    #[test]
    fn test_haversine() {
        // Tugu Adipura coords ~ (-5.4289, 105.2618)
        let d = distance_meters(-5.4289, 105.2618, -5.4289, 105.2618);
        assert!(d < 1.0);
    }

    #[test]
    fn test_classification_ptz() {
        let c = Camera {
            id: "sw-19".into(),
            source: Source::SeribuWajah,
            source_key: "19".into(),
            name: "JEMBATAN MILLENIAL 2 (PTZ)".into(),
            city_address: None,
            district: None,
            subdistrict: None,
            category: Some("Jalan Protokol".into()),
            latitude: -5.429,
            longitude: 105.261,
            playback: Playback::Hls,
            stream_url: Some("https://example.com/stream.m3u8".into()),
            embed_url: None,
            thumbnail: None,
            token: None,
            status: CameraStatus::Online,
            tags: vec![],
        };
        let tags = classify(&c);
        assert!(tags.contains(&Tag::Macet));
        assert!(tags.contains(&Tag::Ptz));
    }
}
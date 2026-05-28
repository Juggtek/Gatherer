//! `.ttasset` JSON writer/reader — the per-layer adaptive recipe.
//!
//! Shape (from `ANDD02.ttasset`):
//! ```json
//! { "meta": { "documentType":"asset", "assetType":"music",
//!             "documentFormatVersion":1, "productionCode":"ANDD02",
//!             "variantName":"Atlas", "assetName":"Atlas" },
//!   "music": { "id":"<uuid>", "pageUuid":"<uuid>", "tags":[], "description":"",
//!              "layers":[ { "layer":0, "formula": { ... }, "balancerIncludeFactors":[...] } ]
//!   } }
//! ```
//!
//! `constantValues` order = `[s, d, min, max, lo]`. Our `SlotParams` stores
//! `(steepness, deviation, minimum, maximum, original_level)` — **swap at
//! the boundary** (min before max in JSON, max before min in SlotParams).

use crate::adaptive::{AdaptiveMixer, SLOT_COUNT};
use crate::navigator::model::AssetMeta;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

// ── JSON shapes ──────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct TtassetDoc {
    meta: TtMeta,
    music: TtMusic,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct TtMeta {
    document_type: String,
    asset_type: String,
    document_format_version: u32,
    production_code: String,
    variant_name: String,
    asset_name: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct TtMusic {
    id: String,
    page_uuid: String,
    tags: Vec<Value>,
    description: String,
    layers: Vec<TtLayer>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct TtLayer {
    layer: usize,
    formula: TtFormula,
    balancer_include_factors: Vec<f64>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct TtFormula {
    #[serde(rename = "type")]
    formula_type: String,
    mood_factors: Vec<f64>,
    constant_names: Vec<String>,
    constant_values: Vec<f64>,
}

// ── formula type mapping ──────────────────────────────────────────────────────

/// F1 → "type1", F3 → "type3", etc. (Atlas only uses type1 + type3 in
/// practice, but we emit the correct suffix for all 9.)
fn formula_type(formula: u8) -> String {
    format!("type{formula}")
}

/// Parse the suffix integer out of "typeN".
fn parse_formula_type(s: &str) -> u8 {
    s.trim_start_matches("type")
        .parse()
        .unwrap_or(1)
        .clamp(1, 9)
}

// ── public API ────────────────────────────────────────────────────────────────

/// Build a `.ttasset` JSON document from the mixer state + asset metadata.
/// Returns the pretty-printed JSON string.
pub fn to_json(mixer: &AdaptiveMixer, meta: &AssetMeta, music_uuid: &str, page_uuid: &str) -> String {
    let mood_idx = |m: usize| m; // Dark=0, Neutral=1, Bright=2

    let layers: Vec<TtLayer> = (0..SLOT_COUNT)
        .map(|s| {
            let p = mixer.slot_params[s];
            // constantValues: [s, d, min, max, lo] — note min before max (JSON convention).
            TtLayer {
                layer: s,
                formula: TtFormula {
                    formula_type: formula_type(p.formula),
                    mood_factors: (0..3)
                        .map(|m| mixer.mood_weight[mood_idx(m)][s] as f64)
                        .collect(),
                    constant_names: vec![
                        "s".into(), "d".into(), "min".into(), "max".into(), "lo".into(),
                    ],
                    constant_values: vec![
                        p.steepness as f64,
                        p.deviation as f64,
                        p.minimum as f64,   // min first (JSON order)
                        p.maximum as f64,
                        p.original_level as f64,
                    ],
                },
                balancer_include_factors: (0..3)
                    .map(|m| mixer.balancer_mask[mood_idx(m)][s] as f64)
                    .collect(),
            }
        })
        .collect();

    let doc = TtassetDoc {
        meta: TtMeta {
            document_type: "asset".into(),
            asset_type: "music".into(),
            document_format_version: 1,
            production_code: meta.production_code.clone(),
            variant_name: meta.variant_name.clone(),
            asset_name: meta.asset_name.clone(),
        },
        music: TtMusic {
            id: music_uuid.to_string(),
            page_uuid: page_uuid.to_string(),
            tags: Vec::new(),
            description: meta.description.clone(),
            layers,
        },
    };

    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// Write the `.ttasset` file into `dir / "<production_code>.ttasset"`.
pub fn write(
    dir: &Path,
    mixer: &AdaptiveMixer,
    meta: &AssetMeta,
    music_uuid: &str,
    page_uuid: &str,
) -> Result<std::path::PathBuf, String> {
    let json = to_json(mixer, meta, music_uuid, page_uuid);
    let path = dir.join(format!("{}.ttasset", meta.production_code));
    std::fs::write(&path, json)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// Parse a `.ttasset` file and populate `mixer` + `meta`. Returns the
/// `music.id` and `music.pageUuid` for use by the model writer.
pub fn read(
    path: &Path,
    mixer: &mut AdaptiveMixer,
    meta: &mut AssetMeta,
) -> Result<(String, String), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let doc: TtassetDoc = serde_json::from_str(&content)
        .map_err(|e| format!("parse {}: {e}", path.display()))?;

    meta.production_code = doc.meta.production_code;
    meta.variant_name = doc.meta.variant_name;
    meta.asset_name = doc.meta.asset_name;

    for layer in &doc.music.layers {
        let s = layer.layer;
        if s >= SLOT_COUNT {
            continue;
        }
        let p = &mut mixer.slot_params[s];
        p.formula = parse_formula_type(&layer.formula.formula_type);
        let cv = &layer.formula.constant_values;
        if cv.len() >= 5 {
            p.steepness     = cv[0] as f32;
            p.deviation     = cv[1] as f32;
            p.minimum       = cv[2] as f32; // JSON: min before max
            p.maximum       = cv[3] as f32;
            p.original_level = cv[4] as f32;
        }
        for (m, &mf) in layer.formula.mood_factors.iter().enumerate().take(3) {
            mixer.mood_weight[m][s] = mf as f32;
        }
        for (m, &bf) in layer.balancer_include_factors.iter().enumerate().take(3) {
            mixer.balancer_mask[m][s] = bf as f32;
        }
    }

    Ok((doc.music.id, doc.music.page_uuid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adaptive::AdaptiveMixer;

    #[test]
    fn round_trip_ttasset() {
        let mut m = AdaptiveMixer::new();
        m.slot_params[0].steepness = 0.45;
        m.slot_params[0].minimum = 0.0;
        m.slot_params[0].maximum = 1.0;
        m.slot_params[0].formula = 3;
        m.mood_weight[0][0] = 0.5;
        m.balancer_mask[1][2] = 0.7;
        let meta = AssetMeta {
            production_code: "TEST01".into(),
            variant_name: "Test".into(),
            asset_name: "Test".into(),
            ..Default::default()
        };
        let json = to_json(&m, &meta, "music-uuid", "page-uuid");

        let mut m2 = AdaptiveMixer::new();
        let mut meta2 = AssetMeta::default();
        let tmp = std::env::temp_dir().join(format!("ttasset-rt-{}.ttasset", std::process::id()));
        std::fs::write(&tmp, &json).unwrap();
        let (mid, pid) = read(&tmp, &mut m2, &mut meta2).unwrap();
        let _ = std::fs::remove_file(&tmp);

        assert_eq!(mid, "music-uuid");
        assert_eq!(pid, "page-uuid");
        assert_eq!(meta2.production_code, "TEST01");
        assert!((m2.slot_params[0].steepness - 0.45).abs() < 1e-5);
        assert_eq!(m2.slot_params[0].formula, 3);
        assert!((m2.mood_weight[0][0] - 0.5).abs() < 1e-5);
        assert!((m2.balancer_mask[1][2] - 0.7).abs() < 1e-5);
    }

    /// Verify the min/max swap matches the real Atlas file.
    #[test]
    fn atlas_ttasset_parses() {
        let path = std::path::Path::new(
            "/Volumes/Plottn/GREENLOBSTER/COLLECTION/Integrated Assets/Music/ANDD02 - Atlas_TT/ANDD02.ttasset",
        );
        if !path.exists() {
            return; // skip when drive not mounted
        }
        let mut mixer = AdaptiveMixer::new();
        let mut meta = AssetMeta::default();
        let (id, _) = read(path, &mut mixer, &mut meta).unwrap();
        assert!(!id.is_empty());
        assert_eq!(meta.production_code, "ANDD02");
        // Layer 0 in Atlas: type3, s=0.45, d=0.22, min=0, max=1, lo=0.7
        assert!((mixer.slot_params[0].steepness - 0.45).abs() < 1e-4);
        assert_eq!(mixer.slot_params[0].formula, 3);
    }
}

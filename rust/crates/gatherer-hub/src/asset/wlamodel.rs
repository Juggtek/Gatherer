//! `.wlamodel` JSON writer/reader.
//!
//! The file holds a `content` array of typed objects: 1 `page`, 1 `module`,
//! then N `pattern` entries (one per section mix + per layer stem).
//!
//! Key mappings vs our Region:
//! - `begin` / `end`     → `begin_frames` / `end_frames` (sample frames)
//! - `fadePercentage`    → `fade_pct`
//! - `fadeShape`         → `fade_shape`
//! The `sync` field from `NavigatorRegion` is not in the wlamodel — the
//! engine derives sync = begin (out) / end (in).

use crate::navigator::model::{Region, SectionKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

// ── public input types ───────────────────────────────────────────────────────

pub struct SectionSpec<'a> {
    pub kind: SectionKind,
    /// Pattern id for the mixed section clip (e.g. "i1", "m1", "o1").
    pub pattern_id: &'a str,
    /// Length in sample frames.
    pub len_frames: u64,
    pub bank_name: &'a str,
    pub in_regions: &'a [Region],
    pub out_regions: &'a [Region],
    /// Per-layer stem ids (empty → no layer patterns).
    pub layer_ids: Vec<String>,
}

// ── JSON region shape ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ModelRegion {
    pub begin: u64,
    pub end: u64,
    pub fade_percentage: f64,
    pub fade_shape: f64,
    pub group: u32,
}

impl From<&Region> for ModelRegion {
    fn from(r: &Region) -> Self {
        ModelRegion {
            begin: r.begin_frames,
            end: r.end_frames,
            fade_percentage: r.fade_pct as f64,
            fade_shape: r.fade_shape as f64,
            group: r.group,
        }
    }
}

impl From<&ModelRegion> for Region {
    fn from(r: &ModelRegion) -> Self {
        let sync = r.begin; // out-region default
        Region {
            begin_frames: r.begin,
            end_frames: r.end,
            sync_frames: sync,
            fade_pct: r.fade_percentage as f32,
            fade_shape: r.fade_shape as f32,
            group: r.group,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ModelLoop {
    begin: u64,
    enabled: bool,
    end: u64,
    xfade: u64,
    xoffset: i64,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct AudioContent {
    bank_name: String,
    clip_name: String,
    reading_method: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ChannelStrip {
    pan: f64,
    volume: f64,
    width: f64,
}

impl Default for ChannelStrip {
    fn default() -> Self {
        Self { pan: 0.5, volume: 1.0, width: 1.0 }
    }
}

// ── writers ──────────────────────────────────────────────────────────────────

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn pattern_entry(
    pattern_id: &str,
    clip_name: &str,
    bank_name: &str,
    loop_range: Option<(u64, u64)>,
    in_regions: &[Region],
    out_regions: &[Region],
) -> Value {
    let loop_obj = if let Some((begin, end)) = loop_range {
        json!({ "begin": begin, "enabled": true, "end": end, "xfade": 0, "xoffset": 0 })
    } else {
        json!({ "begin": 0, "enabled": true, "end": 0, "xfade": 0, "xoffset": 0 })
    };
    let in_r: Vec<Value> = in_regions
        .iter()
        .map(|r| {
            json!({
                "begin": r.begin_frames, "end": r.end_frames,
                "fadePercentage": r.fade_pct, "fadeShape": r.fade_shape, "group": r.group
            })
        })
        .collect();
    let out_r: Vec<Value> = out_regions
        .iter()
        .map(|r| {
            json!({
                "begin": r.begin_frames, "end": r.end_frames,
                "fadePercentage": r.fade_pct, "fadeShape": r.fade_shape, "group": r.group
            })
        })
        .collect();
    json!({
        "type": "pattern",
        "data": {
            "allowZones": [],
            "audioContent": {
                "bankName": bank_name,
                "clipName": format!("module_{bank_name}/{clip_name}"),
                "readingMethod": "Streamed"
            },
            "channelStrip": { "pan": 0.5, "volume": 1.0, "width": 1.0 },
            "group": 0,
            "id": pattern_id,
            "inRegions": in_r,
            "loop": loop_obj,
            "outRegions": out_r,
            "parameters": [],
            "plugins": [],
            "uuid": new_uuid()
        }
    })
}

/// Build and write a `.wlamodel` for a music asset. `sections` must be in
/// Intro→Main→Outro order; each carries its mix-pattern id + per-layer ids.
/// Returns the written path.
pub fn write(
    dir: &Path,
    production_code: &str,
    module_uuid: &str,
    page_uuid: &str,
    sections: &[SectionSpec<'_>],
    sample_rate: u32,
) -> Result<std::path::PathBuf, String> {
    let json_str = to_json(production_code, module_uuid, page_uuid, sections, sample_rate);
    let path = dir.join(format!("{production_code}.wlamodel"));
    std::fs::write(&path, json_str)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

pub fn to_json(
    production_code: &str,
    module_uuid: &str,
    page_uuid: &str,
    sections: &[SectionSpec<'_>],
    _sample_rate: u32,
) -> String {
    let mut content: Vec<Value> = Vec::new();
    let mut intro_patterns: Vec<Value> = Vec::new();
    let mut main_patterns: Vec<Value> = Vec::new();
    let mut outro_patterns: Vec<Value> = Vec::new();
    let layer_patterns: Vec<Value>;
    let mut layer_mappings: Vec<Value> = Vec::new();
    let mut all_pattern_uuids: Vec<Value> = Vec::new();

    for sec in sections {
        // Main mix pattern (section combined audio).
        let loop_range = if sec.kind == SectionKind::Main {
            Some((0u64, sec.len_frames))
        } else {
            None
        };
        let mix_entry = pattern_entry(
            sec.pattern_id,
            sec.pattern_id,
            sec.bank_name,
            loop_range,
            sec.in_regions,
            sec.out_regions,
        );
        let mix_uuid = mix_entry["data"]["uuid"].as_str().unwrap_or("").to_string();
        all_pattern_uuids.push(json!({ "uuref": mix_uuid }));

        let ref_entry = json!({ "uuref": mix_uuid });
        match sec.kind {
            SectionKind::Intro => intro_patterns.push(ref_entry),
            SectionKind::Main  => main_patterns.push(ref_entry),
            SectionKind::Outro => outro_patterns.push(ref_entry),
        }
        content.push(mix_entry);

        // Per-layer stem patterns (no in/out regions — mix carries them).
        let mut layer_ids_in_mapping: Vec<Value> = Vec::new();
        for lid in &sec.layer_ids {
            let entry = pattern_entry(
                lid,
                lid,
                sec.bank_name,
                loop_range,
                &[],
                &[],
            );
            let luuid = entry["data"]["uuid"].as_str().unwrap_or("").to_string();
            all_pattern_uuids.push(json!({ "uuref": luuid }));
            layer_ids_in_mapping.push(json!({ "id": lid }));
            content.push(entry);
        }
        if !layer_ids_in_mapping.is_empty() {
            layer_mappings.push(json!({
                "patternId": sec.pattern_id,
                "layers": layer_ids_in_mapping
            }));
        }
    }

    // layerPatterns uuref list mirrors all patterns in order.
    layer_patterns = all_pattern_uuids.clone();

    // Module entry.
    let module_entry = json!({
        "type": "module",
        "data": {
            "channelStrip": { "pan": 0.5, "volume": 1.0, "width": 1.0 },
            "id": production_code,
            "introPatterns": intro_patterns,
            "layerMappings": layer_mappings,
            "layerPatterns": layer_patterns,
            "mainPatterns": main_patterns,
            "outroPatterns": outro_patterns,
            "parameters": [],
            "plugins": [],
            "scripts": null,
            "uuid": module_uuid
        }
    });

    // Page entry.
    let page_entry = json!({
        "type": "page",
        "data": {
            "channelStrip": { "pan": 0.5, "volume": 1.0, "width": 1.0 },
            "id": production_code,
            "modules": [{ "uuref": module_uuid }],
            "parameters": [],
            "plugins": [],
            "scripts": null,
            "uuid": page_uuid
        }
    });

    // content = [page, module, ...patterns]
    let mut full_content = vec![page_entry, module_entry];
    full_content.extend(content);

    let doc = json!({
        "content": full_content,
        "document": {
            "createdAt": chrono_now(),
            "createdBy": "Gatherer Hub 1.0",
            "version": 1
        }
    });

    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

fn chrono_now() -> String {
    // Simple RFC-3339 without chrono dep: best-effort from SystemTime.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // naïve UTC, no DST handling — good enough for a bundle timestamp.
    let (y, mo, d, h, min, s) = epoch_to_parts(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{min:02}:{s:02}+00:00")
}

fn epoch_to_parts(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let s = secs % 60;
    let mins = secs / 60;
    let min = mins % 60;
    let hours = mins / 60;
    let h = hours % 24;
    let days = hours / 24;
    // Approximate date from day count (Gregorian, good 1970-2100).
    let mut y = 1970u64;
    let mut rem = days;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let dy = if leap { 366 } else { 365 };
        if rem < dy {
            break;
        }
        rem -= dy;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let month_days = [31u64, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 1u64;
    for md in month_days {
        if rem < md {
            break;
        }
        rem -= md;
        mo += 1;
    }
    (y, mo, rem + 1, h, min, s)
}

/// Parse a `.wlamodel` file and return per-section info: for each pattern
/// whose id matches `{i,m,o}N`, extract its regions + length.
pub struct ParsedSection {
    pub kind: SectionKind,
    pub pattern_id: String,
    pub len_frames: u64,
    pub in_regions: Vec<Region>,
    pub out_regions: Vec<Region>,
    pub layer_ids: Vec<String>,
}

pub fn read(path: &Path) -> Result<(String, String, Vec<ParsedSection>), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let doc: Value = serde_json::from_str(&content)
        .map_err(|e| format!("parse {}: {e}", path.display()))?;

    let items = doc["content"].as_array().ok_or("missing content")?;

    // Find module and page uuids.
    let module = items.iter()
        .find(|c| c["type"] == "module")
        .and_then(|c| c["data"].as_object())
        .ok_or("missing module")?;
    let module_uuid = module["uuid"].as_str().unwrap_or("").to_string();
    let page_uuid = items.iter()
        .find(|c| c["type"] == "page")
        .and_then(|c| c["data"]["uuid"].as_str())
        .unwrap_or("")
        .to_string();

    // Build a map of pattern id → uuid (for layer mapping).
    let layer_mappings: Vec<(String, Vec<String>)> = module
        .get("layerMappings")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|lm| {
                    let pid = lm["patternId"].as_str()?.to_string();
                    let layers: Vec<String> = lm["layers"]
                        .as_array()
                        .map(|la| {
                            la.iter()
                                .filter_map(|l| l["id"].as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    Some((pid, layers))
                })
                .collect()
        })
        .unwrap_or_default();

    // Identify section kinds from module's intro/main/outroPatterns uurefs.
    let extract_uuids = |key: &str| -> Vec<String> {
        module.get(key)
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|r| r["uuref"].as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default()
    };
    let intro_uuids = extract_uuids("introPatterns");
    let main_uuids  = extract_uuids("mainPatterns");
    let outro_uuids = extract_uuids("outroPatterns");

    let mut uuid_to_pattern: std::collections::HashMap<String, (String, u64, Vec<Region>, Vec<Region>)> = Default::default();
    for item in items {
        if item["type"] != "pattern" { continue; }
        let d = &item["data"];
        let uuid = d["uuid"].as_str().unwrap_or("").to_string();
        let pid  = d["id"].as_str().unwrap_or("").to_string();
        let len  = d["loop"]["end"].as_u64().unwrap_or(0);
        let in_r: Vec<Region> = d["inRegions"].as_array()
            .map(|a| a.iter().map(|r| Region {
                begin_frames: r["begin"].as_u64().unwrap_or(0),
                end_frames:   r["end"].as_u64().unwrap_or(0),
                sync_frames:  r["end"].as_u64().unwrap_or(0), // in-region: sync at end
                fade_pct:     r["fadePercentage"].as_f64().unwrap_or(0.0) as f32,
                fade_shape:   r["fadeShape"].as_f64().unwrap_or(0.5) as f32,
                group:        r["group"].as_u64().unwrap_or(0) as u32,
            }).collect())
            .unwrap_or_default();
        let out_r: Vec<Region> = d["outRegions"].as_array()
            .map(|a| a.iter().map(|r| Region {
                begin_frames: r["begin"].as_u64().unwrap_or(0),
                end_frames:   r["end"].as_u64().unwrap_or(0),
                sync_frames:  r["begin"].as_u64().unwrap_or(0), // out-region: sync at begin
                fade_pct:     r["fadePercentage"].as_f64().unwrap_or(0.0) as f32,
                fade_shape:   r["fadeShape"].as_f64().unwrap_or(0.5) as f32,
                group:        r["group"].as_u64().unwrap_or(0) as u32,
            }).collect())
            .unwrap_or_default();
        uuid_to_pattern.insert(uuid, (pid, len, in_r, out_r));
    }

    let mut sections = Vec::new();
    let kinds_and_uuids = [
        (SectionKind::Intro, &intro_uuids),
        (SectionKind::Main,  &main_uuids),
        (SectionKind::Outro, &outro_uuids),
    ];
    for (kind, uuids) in kinds_and_uuids {
        for uuid in uuids {
            if let Some((pid, len, in_r, out_r)) = uuid_to_pattern.get(uuid) {
                let layer_ids = layer_mappings.iter()
                    .find(|(p, _)| p == pid)
                    .map(|(_, ids)| ids.clone())
                    .unwrap_or_default();
                sections.push(ParsedSection {
                    kind,
                    pattern_id: pid.clone(),
                    len_frames: *len,
                    in_regions: in_r.clone(),
                    out_regions: out_r.clone(),
                    layer_ids,
                });
            }
        }
    }

    Ok((module_uuid, page_uuid, sections))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atlas_wlamodel_parses() {
        let path = Path::new(
            "/Volumes/Plottn/GREENLOBSTER/COLLECTION/Integrated Assets/Music/ANDD02 - Atlas_TT/ANDD02.wlamodel",
        );
        if !path.exists() { return; }
        let (_, _, sections) = read(path).unwrap();
        let kinds: Vec<_> = sections.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&SectionKind::Intro));
        assert!(kinds.contains(&SectionKind::Main));
        assert!(kinds.contains(&SectionKind::Outro));
        let main = sections.iter().find(|s| s.kind == SectionKind::Main).unwrap();
        assert_eq!(main.out_regions.len(), 38, "Atlas main has 38 outRegions");
        // Verify out-region sync is at begin.
        assert_eq!(main.out_regions[0].sync_frames, main.out_regions[0].begin_frames);
    }

    #[test]
    fn write_and_parse_roundtrip() {
        let dir = std::env::temp_dir().join(format!("wlamodel-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r_out = Region::new_out(1000, 2000, 1.0, 0.756);
        let r_in  = Region::new_in(0, 500, 0.0, 0.5);
        let intro_out = [r_out];
        let intro_in  = [r_in];
        let main_out  = [r_out];
        let secs = vec![
            SectionSpec {
                kind: SectionKind::Intro,
                pattern_id: "i1",
                len_frames: 2000,
                bank_name: "TEST.wlabank",
                in_regions: &intro_in,
                out_regions: &intro_out,
                layer_ids: vec!["i1l1".into()],
            },
            SectionSpec {
                kind: SectionKind::Main,
                pattern_id: "m1",
                len_frames: 8000,
                bank_name: "TEST.wlabank",
                in_regions: &[],
                out_regions: &main_out,
                layer_ids: vec!["m1l1".into()],
            },
        ];
        write(&dir, "TEST", "mod-uuid", "page-uuid", &secs, 48_000).unwrap();
        let (mod_uuid, page_uuid, parsed) = read(&dir.join("TEST.wlamodel")).unwrap();
        assert_eq!(mod_uuid, "mod-uuid");
        assert_eq!(page_uuid, "page-uuid");
        assert_eq!(parsed.len(), 2);
        let intro = &parsed[0];
        assert_eq!(intro.kind, SectionKind::Intro);
        assert_eq!(intro.out_regions[0].begin_frames, 1000);
        assert_eq!(intro.out_regions[0].sync_frames, 1000); // out sync = begin
        assert_eq!(intro.layer_ids, vec!["i1l1"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

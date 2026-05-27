//! Template I/O — the 97-float `.txt` format from the parent patch
//! `Adaptive-Mixer_1.0.0_mac.maxpat` ("Structure" panel).
//!
//! Layout (positions, whitespace-separated floats):
//!
//! - `[0]`         — version number (the bundled templates ship `0`).
//! - `[1..49]`     — 8 layers × 6 params, in slot order. Per layer:
//!                   `steepness, deviation, maximum, minimum, original_level, formula`.
//! - `[49..57]`    — Dark mood mask (M1, L0..L7).
//! - `[57..65]`    — Dark balancer mask (M2 / "BIF", L0..L7).
//! - `[65..73]`    — Neutral mood mask.
//! - `[73..81]`    — Neutral balancer mask.
//! - `[81..89]`    — Bright mood mask.
//! - `[89..97]`    — Bright balancer mask.
//!
//! Reference files: `Templates/Music_template.txt`,
//! `Templates/Location_template.txt`.

use crate::adaptive::{AdaptiveMixer, Mood, SLOT_COUNT};
use std::fs;
use std::path::{Path, PathBuf};

const NUM_VALUES: usize = 97;
/// Mood order in the template stream. Must stay `Dark, Neutral, Bright`
/// to match the patch's [49..] slab layout — do NOT switch to
/// `Mood::ALL` if its order ever changes.
const MOOD_ORDER: [Mood; 3] = [Mood::Dark, Mood::Neutral, Mood::Bright];

/// Parse a template-file body and copy every field into `mixer` —
/// `slot_params[0..8]`, `mood_weight[mood][slot]`, and
/// `balancer_mask[mood][slot]`. On error nothing is written.
pub fn parse_into(text: &str, mixer: &mut AdaptiveMixer) -> Result<(), String> {
    let values: Vec<f32> = text
        .split_whitespace()
        .map(|t| t.parse::<f32>().map_err(|e| format!("parse `{t}`: {e}")))
        .collect::<Result<_, _>>()?;
    if values.len() != NUM_VALUES {
        return Err(format!(
            "expected {NUM_VALUES} floats (version + 8×6 params + 6×8 masks), got {}",
            values.len()
        ));
    }
    // [0] is the version. The bundled templates use `0`; we don't enforce
    // it (any value is accepted) but we do remember it for round-tripping.
    let _version = values[0];

    // Stage into temporaries so a failure mid-parse leaves `mixer` clean.
    let mut slots = mixer.slot_params; // Copy
    for s in 0..SLOT_COUNT {
        let off = 1 + s * 6;
        let p = &mut slots[s];
        p.steepness = values[off].clamp(0.0, 1.0);
        p.deviation = values[off + 1].clamp(0.0, 1.0);
        p.maximum = values[off + 2].clamp(0.0, 2.0);
        p.minimum = values[off + 3].clamp(0.0, 1.0);
        p.original_level = values[off + 4].clamp(0.0, 1.0);
        // formula is a float in the file ("5." in the patch examples);
        // round + clamp into the patch's 1..=9 switch range.
        p.formula = values[off + 5].round().clamp(1.0, 9.0) as u8;
    }

    let mut mood = mixer.mood_weight;
    let mut bal = mixer.balancer_mask;
    let mut off = 49;
    for &m in MOOD_ORDER.iter() {
        for s in 0..SLOT_COUNT {
            mood[m as usize][s] = values[off + s].clamp(0.0, 1.0);
        }
        off += SLOT_COUNT;
        for s in 0..SLOT_COUNT {
            bal[m as usize][s] = values[off + s].clamp(0.0, 1.0);
        }
        off += SLOT_COUNT;
    }

    mixer.slot_params = slots;
    mixer.mood_weight = mood;
    mixer.balancer_mask = bal;
    Ok(())
}

/// Format `mixer` as a 97-token whitespace-separated string ready to be
/// written to disk. Layout matches `parse_into` so it round-trips.
pub fn format(mixer: &AdaptiveMixer) -> String {
    // ~6 chars per token × 97 + spaces ≈ 700 bytes. Reserve a bit more.
    let mut out = String::with_capacity(900);
    out.push('0'); // version
    for s in 0..SLOT_COUNT {
        let p = mixer.slot_params[s];
        out.push_str(&format!(
            " {:.3} {:.3} {:.3} {:.3} {:.3} {:.3}",
            p.steepness,
            p.deviation,
            p.maximum,
            p.minimum,
            p.original_level,
            p.formula as f32,
        ));
    }
    for &m in MOOD_ORDER.iter() {
        for s in 0..SLOT_COUNT {
            out.push_str(&format!(" {:.3}", mixer.mood_weight[m as usize][s]));
        }
        for s in 0..SLOT_COUNT {
            out.push_str(&format!(" {:.3}", mixer.balancer_mask[m as usize][s]));
        }
    }
    out
}

/// `~/Music/Gatherer/Templates/` — sibling of the per-session folders.
pub fn templates_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join("Music").join("Gatherer").join("Templates"))
}

fn ensure_templates_dir() -> Result<PathBuf, String> {
    let dir = templates_dir().ok_or_else(|| "HOME not set".to_string())?;
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Alphabetical list of every `*.txt` under `templates_dir()`.
#[allow(dead_code)] // ex-pick_list source; kept for a future "Recent templates" menu
pub fn list_templates() -> Vec<String> {
    let Some(dir) = templates_dir() else {
        return Vec::new();
    };
    let Ok(rd) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            (n.ends_with(".txt") && !n.starts_with('.')).then_some(n)
        })
        .collect();
    names.sort_unstable();
    names
}

/// Read `<templates_dir>/<name>` into `mixer`.
#[allow(dead_code)] // ex-pick_list handler; kept paired with `list_templates`
pub fn load_into(name: &str, mixer: &mut AdaptiveMixer) -> Result<PathBuf, String> {
    let dir = templates_dir().ok_or_else(|| "HOME not set".to_string())?;
    let path = dir.join(name);
    parse_file_into(&path, mixer)?;
    Ok(path)
}

/// Read an arbitrary file into `mixer` (useful for tests and for
/// importing the legacy templates that live outside `templates_dir()`).
pub fn parse_file_into(path: &Path, mixer: &mut AdaptiveMixer) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    parse_into(&text, mixer)
}

/// Write `mixer` to `<templates_dir>/<name>.txt`. Returns the written
/// path so the UI can echo it back to the user.
#[allow(dead_code)] // kept as an escape hatch; the UI now writes into the session folder
pub fn save(name: &str, mixer: &AdaptiveMixer) -> Result<PathBuf, String> {
    let dir = ensure_templates_dir()?;
    let file = if name.ends_with(".txt") {
        name.to_string()
    } else {
        format!("{name}.txt")
    };
    let path = dir.join(file);
    save_to(&path, mixer)
}

/// Write `mixer` to an explicit `path` (file, not directory). Creates
/// the parent directory if needed.
pub fn save_to(path: &Path, mixer: &AdaptiveMixer) -> Result<PathBuf, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    fs::write(path, format(mixer)).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path.to_path_buf())
}

// ── last-used import dir (persisted across app launches) ─────────────
//
// Lives at `~/Music/Gatherer/.last_template_dir` — a single line of UTF-8
// holding the path. Plain text on purpose so the user can edit/inspect
// it without a TOML parser in the loop. Best-effort: a missing or
// unreadable file falls back to "no preference".

fn last_dir_marker() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join("Music").join("Gatherer").join(".last_template_dir"))
}

/// Return the directory we last imported a template from (if any and
/// still extant). Used to seed the next file-picker dialog.
pub fn read_last_dir() -> Option<PathBuf> {
    let marker = last_dir_marker()?;
    let s = fs::read_to_string(&marker).ok()?;
    let p = PathBuf::from(s.trim());
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

/// Remember `dir` as the next file-picker's starting location. Silent on
/// I/O failure (this is a UX nicety, not load-bearing).
pub fn write_last_dir(dir: &Path) {
    let Some(marker) = last_dir_marker() else {
        return;
    };
    if let Some(parent) = marker.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&marker, dir.display().to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> AdaptiveMixer {
        let mut m = AdaptiveMixer::new();
        for i in 0..SLOT_COUNT {
            m.slot_params[i].steepness = 0.1 * (i as f32 + 1.0);
            m.slot_params[i].deviation = 0.02 * (i as f32 + 1.0);
            m.slot_params[i].maximum = 1.0;
            m.slot_params[i].minimum = 0.05 * (i as f32);
            m.slot_params[i].original_level = 0.3;
            m.slot_params[i].formula = ((i % 9) + 1) as u8;
        }
        for mood in 0..3 {
            for s in 0..SLOT_COUNT {
                m.mood_weight[mood][s] = (s as f32) / 8.0;
                m.balancer_mask[mood][s] = 1.0 - (s as f32) / 16.0;
            }
        }
        m
    }

    #[test]
    fn roundtrip_format_parse() {
        let a = fixture();
        let text = format(&a);
        let mut b = AdaptiveMixer::new();
        parse_into(&text, &mut b).unwrap();
        for s in 0..SLOT_COUNT {
            assert!((a.slot_params[s].steepness - b.slot_params[s].steepness).abs() < 1e-3);
            assert!((a.slot_params[s].minimum - b.slot_params[s].minimum).abs() < 1e-3);
            assert_eq!(a.slot_params[s].formula, b.slot_params[s].formula);
        }
        for mood in 0..3 {
            for s in 0..SLOT_COUNT {
                assert!((a.mood_weight[mood][s] - b.mood_weight[mood][s]).abs() < 1e-3);
                assert!((a.balancer_mask[mood][s] - b.balancer_mask[mood][s]).abs() < 1e-3);
            }
        }
    }

    #[test]
    fn rejects_wrong_count() {
        let mut m = AdaptiveMixer::new();
        let err = parse_into("0 1 2 3", &mut m).unwrap_err();
        assert!(err.contains("got 4"), "{err}");
    }

    #[test]
    fn parses_music_template_shape() {
        // 1 + 48 + 48 = 97 zeros — every layer collapses to defaults +
        // every mask becomes 0. Should still parse cleanly.
        let zeros: String = std::iter::repeat("0 ").take(NUM_VALUES).collect();
        let mut m = AdaptiveMixer::new();
        parse_into(&zeros, &mut m).unwrap();
        // formula = round(0) = 0, then clamped to 1.
        for s in 0..SLOT_COUNT {
            assert_eq!(m.slot_params[s].formula, 1);
        }
    }
}

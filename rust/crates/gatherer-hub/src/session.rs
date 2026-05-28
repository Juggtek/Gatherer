//! Session save/load — `session.toml` written next to the session's
//! recording folders. Survives app restarts and carries the whole
//! project tree (assets → pages → modules → sections → patterns; see
//! [`crate::navigator::model`]).
//!
//! ## Schema evolution
//!
//! - **v1** — a single top-level `take` (no sections, no project).
//! - **v2** (Phase A) — a flat `sections` list + `asset_meta`, but in
//!   practice the app still wrote the v1 top-level `take`.
//! - **v3** (current) — a full `project` tree. Legacy `take` /
//!   `asset_meta` are still accepted on load via the transitional
//!   fields below and folded into the project by [`migrate_into_project`];
//!   they are never written back.

use crate::navigator::{AssetMeta, ClipSource, Pattern, Project, Section, SectionKind};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const SESSION_FILE: &str = "session.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub session_name: String,
    pub sample_rate: u32,
    pub time_sig_num: u32,
    pub target_lufs: f32,
    pub zoom: f32,
    pub snap_to_grid: bool,
    #[serde(default)]
    pub layer_names: Vec<String>,

    /// The project tree (v3+). Default-empty so older files still load;
    /// [`migrate_into_project`] populates it from the legacy fields.
    #[serde(default)]
    pub project: Project,

    // ── transitional / legacy (migrated on load, never re-saved) ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take: Option<TakeState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_meta: Option<AssetMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakeState {
    pub armed: Vec<usize>,
    pub start_pulses: u64,
    pub bpm: f32,
    pub time_sig_num: u32,
    #[serde(default)]
    pub take_user_offset_units: f32,
}

impl SessionState {
    /// Build a v3 session whose project holds a single recorded take in
    /// the Main section's first pattern. Superseded by
    /// [`with_section_takes`] for the app's multi-section Save path;
    /// retained as a convenience + test helper.
    #[allow(dead_code, clippy::too_many_arguments)]
    pub fn with_single_take(
        session_name: String,
        sample_rate: u32,
        time_sig_num: u32,
        target_lufs: f32,
        zoom: f32,
        snap_to_grid: bool,
        layer_names: Vec<String>,
        take: Option<TakeState>,
    ) -> Self {
        let mut project = Project::reset_to_default();
        if let Some(take) = take {
            if let Some(section) = first_main_section_mut(&mut project) {
                let mut pat = Pattern::new("Pattern 1");
                pat.clip_source = Some(ClipSource {
                    path: PathBuf::new(),
                    take: Some(take),
                });
                section.patterns.push(pat);
            }
        }
        Self {
            session_name,
            sample_rate,
            time_sig_num,
            target_lufs,
            zoom,
            snap_to_grid,
            layer_names,
            project,
            take: None,
            asset_meta: None,
        }
    }

    /// Build a v3 session from a set of per-section recorded takes. The
    /// project gets one Asset/Page/Module; each `(kind, take)` becomes a
    /// section (canonical order) holding one pattern whose
    /// `clip_source.take` carries the metadata. A Main section is always
    /// present even if it has no take.
    #[allow(clippy::too_many_arguments)]
    pub fn with_section_takes(
        session_name: String,
        sample_rate: u32,
        time_sig_num: u32,
        target_lufs: f32,
        zoom: f32,
        snap_to_grid: bool,
        layer_names: Vec<String>,
        takes: Vec<(SectionKind, TakeState)>,
    ) -> Self {
        let mut project = Project::reset_to_default();
        let module_uuid = project.assets[0].pages[0].modules[0].uuid;
        for (kind, take) in takes {
            // Ensure the section exists (reset_to_default seeds Main).
            let section_uuid = {
                let module = &project.assets[0].pages[0].modules[0];
                module
                    .sections
                    .iter()
                    .find(|s| s.kind == kind)
                    .map(|s| s.uuid)
            };
            let section_uuid = match section_uuid {
                Some(u) => u,
                None => project.add_section(module_uuid, kind).unwrap(),
            };
            if let Some(section) = project.find_section_mut(section_uuid) {
                let mut pat = Pattern::new("Pattern 1");
                pat.clip_source = Some(ClipSource {
                    path: PathBuf::new(),
                    take: Some(take),
                });
                section.patterns.push(pat);
            }
        }
        Self {
            session_name,
            sample_rate,
            time_sig_num,
            target_lufs,
            zoom,
            snap_to_grid,
            layer_names,
            project,
            take: None,
            asset_meta: None,
        }
    }

    /// Extract per-section takes from the first module — the inverse of
    /// [`with_section_takes`], used by the app's Load path to repopulate
    /// its live per-section take map.
    pub fn section_takes(&self) -> Vec<(SectionKind, TakeState)> {
        let mut out = Vec::new();
        if let Some(module) = self
            .project
            .assets
            .first()
            .and_then(|a| a.pages.first())
            .and_then(|p| p.modules.first())
        {
            for section in &module.sections {
                if let Some(take) = section
                    .patterns
                    .iter()
                    .find_map(|pat| pat.clip_source.as_ref().and_then(|cs| cs.take.clone()))
                {
                    out.push((section.kind, take));
                }
            }
        }
        out
    }

    /// The session's primary recorded take — first pattern of the first
    /// Main section that carries one. Superseded by [`section_takes`];
    /// retained as a convenience + test helper.
    #[allow(dead_code)]
    pub fn primary_take(&self) -> Option<&TakeState> {
        for a in &self.project.assets {
            for p in &a.pages {
                for m in &p.modules {
                    for s in &m.sections {
                        for pat in &s.patterns {
                            if let Some(cs) = &pat.clip_source {
                                if let Some(t) = &cs.take {
                                    return Some(t);
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }
}

/// First Main section in the project (used by the single-take helper).
fn first_main_section_mut(project: &mut Project) -> Option<&mut Section> {
    for a in &mut project.assets {
        for p in &mut a.pages {
            for m in &mut p.modules {
                if let Some(s) = m.sections.iter_mut().find(|s| s.kind == SectionKind::Main) {
                    return Some(s);
                }
            }
        }
    }
    None
}

/// Fold any legacy `take` / `asset_meta` into `project`. No-op when the
/// project is already populated (v3). Called from [`load`].
pub fn migrate_into_project(state: &mut SessionState) {
    if !state.project.is_empty() {
        // Already v3 — drop any stray legacy fields.
        state.take = None;
        state.asset_meta = None;
        return;
    }
    let legacy_take = state.take.take();
    let legacy_meta = state.asset_meta.take();
    if legacy_take.is_none() && legacy_meta.is_none() {
        // Nothing to migrate; leave the project empty (the app seeds a
        // default tree when it needs one).
        return;
    }
    let mut project = Project::reset_to_default();
    if let Some(meta) = legacy_meta {
        if let Some(a) = project.assets.first_mut() {
            a.meta = meta;
        }
    }
    if let Some(take) = legacy_take {
        if let Some(section) = first_main_section_mut(&mut project) {
            let mut pat = Pattern::new("Pattern 1");
            pat.clip_source = Some(ClipSource {
                path: PathBuf::new(),
                take: Some(take),
            });
            section.patterns.push(pat);
        }
    }
    state.project = project;
}

pub fn save(session_root: &Path, state: &SessionState) -> Result<(), String> {
    fs::create_dir_all(session_root)
        .map_err(|e| format!("create {}: {e}", session_root.display()))?;
    let toml_str = toml::to_string_pretty(state).map_err(|e| format!("serialize: {e}"))?;
    let path = session_root.join(SESSION_FILE);
    fs::write(&path, toml_str).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

pub fn load(session_root: &Path) -> Result<SessionState, String> {
    let path = session_root.join(SESSION_FILE);
    let content =
        fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut state: SessionState =
        toml::from_str(&content).map_err(|e| format!("parse {}: {e}", path.display()))?;
    migrate_into_project(&mut state);
    Ok(state)
}

/// `~/Music/Gatherer/<name>/`, created.
pub fn ensure_root(name: &str) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME not set".to_string())?;
    let root = home.join("Music").join("Gatherer").join(name);
    fs::create_dir_all(&root).map_err(|e| format!("create {}: {e}", root.display()))?;
    Ok(root)
}

/// Resolve `<name>` → `~/Music/Gatherer/<name>/` without creating it.
pub fn root_for(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join("Music").join("Gatherer").join(name))
}

/// Names of every directory under `~/Music/Gatherer/` (alphabetical).
pub fn list_sessions() -> Vec<String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let parent = PathBuf::from(home).join("Music").join("Gatherer");
    let Ok(rd) = fs::read_dir(&parent) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().ok().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort_unstable();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gatherer-session-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn demo_take() -> TakeState {
        TakeState {
            armed: vec![0, 1],
            start_pulses: 1234,
            bpm: 120.0,
            time_sig_num: 5,
            take_user_offset_units: -0.25,
        }
    }

    #[test]
    fn roundtrip_single_take_into_project() {
        let dir = temp_dir("take");
        let s = SessionState::with_single_take(
            "demo".into(),
            48_000,
            5,
            -14.0,
            1.5,
            false,
            vec!["Kick".into(), "Bass".into()],
            Some(demo_take()),
        );
        save(&dir, &s).unwrap();
        let back = load(&dir).unwrap();
        assert_eq!(back.session_name, "demo");
        assert_eq!(back.layer_names, vec!["Kick", "Bass"]);
        // Stored in the project tree, reachable via primary_take().
        assert!(back.take.is_none());
        let t = back.primary_take().expect("take in project");
        assert_eq!(t.armed, vec![0, 1]);
        assert_eq!(t.start_pulses, 1234);
        assert!((t.take_user_offset_units - (-0.25)).abs() < 1e-6);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_without_take_keeps_empty_project() {
        let dir = temp_dir("notake");
        let s = SessionState::with_single_take(
            "fresh".into(),
            48_000,
            4,
            -23.0,
            1.0,
            true,
            vec![],
            None,
        );
        save(&dir, &s).unwrap();
        let back = load(&dir).unwrap();
        assert!(back.primary_take().is_none());
        assert!(back.snap_to_grid);
        // reset_to_default project: 1 asset / 1 page / 1 module / 1 Main.
        assert_eq!(back.project.assets.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_v1_legacy_take_into_project_tree() {
        // Hand-crafted v1 session.toml (pre-project schema).
        let dir = temp_dir("legacy");
        let toml = r#"
session_name = "old"
sample_rate = 48000
time_sig_num = 4
target_lufs = -14.0
zoom = 1.0
snap_to_grid = true
layer_names = ["Kick"]

[take]
armed = [0]
start_pulses = 42
bpm = 96.0
time_sig_num = 4
take_user_offset_units = 0.0
"#;
        fs::write(dir.join(SESSION_FILE), toml).unwrap();
        let back = load(&dir).unwrap();
        // Migrated into a default project with the take in Main.
        assert_eq!(back.project.assets.len(), 1);
        assert!(back.take.is_none());
        let t = back.primary_take().expect("legacy take migrated");
        assert_eq!(t.start_pulses, 42);
        // Re-saving must not emit a top-level [take].
        save(&dir, &back).unwrap();
        let again = fs::read_to_string(dir.join(SESSION_FILE)).unwrap();
        assert!(
            !again.contains("\n[take]"),
            "re-save must not emit top-level [take]; got:\n{again}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_legacy_asset_meta_into_first_asset() {
        let dir = temp_dir("meta");
        let toml = r#"
session_name = "atlas"
sample_rate = 48000
time_sig_num = 4
target_lufs = -14.0
zoom = 1.0
snap_to_grid = true
layer_names = []

[asset_meta]
production_code = "ANDD02"
variant_name = "Atlas"
asset_name = "Atlas"
asset_type = "music"
description = ""
tags = []
"#;
        fs::write(dir.join(SESSION_FILE), toml).unwrap();
        let back = load(&dir).unwrap();
        assert_eq!(back.project.assets.len(), 1);
        assert_eq!(back.project.assets[0].meta.production_code, "ANDD02");
        assert_eq!(back.project.assets[0].meta.variant_name, "Atlas");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_multiple_section_takes() {
        // B0's persistence spine: three recorded sections survive
        // save→load with their per-section metadata + canonical order.
        let dir = temp_dir("sections");
        let mk = |pulses: u64, bpm: f32| TakeState {
            armed: vec![0, 2],
            start_pulses: pulses,
            bpm,
            time_sig_num: 4,
            take_user_offset_units: 0.0,
        };
        let s = SessionState::with_section_takes(
            "atlas".into(),
            48_000,
            4,
            -14.0,
            1.0,
            true,
            vec!["L0".into()],
            vec![
                (SectionKind::Outro, mk(900, 100.0)),
                (SectionKind::Intro, mk(100, 120.0)),
                (SectionKind::Main, mk(500, 110.0)),
            ],
        );
        save(&dir, &s).unwrap();
        let back = load(&dir).unwrap();
        let takes = back.section_takes();
        // Canonical order regardless of insertion order.
        let kinds: Vec<_> = takes.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            kinds,
            vec![SectionKind::Intro, SectionKind::Main, SectionKind::Outro]
        );
        // Per-section metadata preserved.
        let main = takes.iter().find(|(k, _)| *k == SectionKind::Main).unwrap();
        assert_eq!(main.1.start_pulses, 500);
        assert!((main.1.bpm - 110.0).abs() < 1e-6);
        assert_eq!(main.1.armed, vec![0, 2]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_v3_project_with_patterns() {
        use crate::navigator::model::Region;
        let dir = temp_dir("v3");
        let mut s = SessionState::with_single_take(
            "atlas".into(),
            48_000,
            4,
            -14.0,
            1.0,
            true,
            vec!["L0".into()],
            Some(demo_take()),
        );
        // Author a richer tree: add intro + outro, a region on Main.
        let module = s.project.assets[0].pages[0].modules[0].uuid;
        s.project.add_section(module, SectionKind::Intro);
        s.project.add_section(module, SectionKind::Outro);
        let main_section = s.project.assets[0].pages[0].modules[0]
            .sections
            .iter()
            .find(|sec| sec.kind == SectionKind::Main)
            .unwrap()
            .uuid;
        let pat = s.project.find_section(main_section).unwrap();
        let pat_uuid = s.project.assets[pat.asset].pages[pat.page].modules[pat.module]
            .sections[pat.section]
            .patterns[0]
            .uuid;
        s.project
            .add_out_region(pat_uuid, Region::new_out(829_487, 863_971, 1.0, 0.756));

        save(&dir, &s).unwrap();
        let back = load(&dir).unwrap();
        let kinds: Vec<_> = back.project.assets[0].pages[0].modules[0]
            .sections
            .iter()
            .map(|sec| sec.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![SectionKind::Intro, SectionKind::Main, SectionKind::Outro]
        );
        let main = &back.project.assets[0].pages[0].modules[0].sections[1];
        assert_eq!(main.patterns[0].out_regions.len(), 1);
        assert_eq!(main.patterns[0].out_regions[0].sync_frames, 829_487);
        let _ = fs::remove_dir_all(&dir);
    }
}

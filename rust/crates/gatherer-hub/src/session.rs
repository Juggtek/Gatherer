//! Session save/load — `session.toml` written next to the session's
//! `recording/` folder. Survives app restarts and lets the user pick up
//! a previously-recorded take with its layer names, drag offset, target
//! LUFS, and so on intact.

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
    #[serde(default)]
    pub take: Option<TakeState>,
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
    toml::from_str(&content).map_err(|e| format!("parse {}: {e}", path.display()))
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

    #[test]
    fn roundtrip_with_take() {
        let dir = std::env::temp_dir().join(format!("gatherer-session-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let s = SessionState {
            session_name: "demo".into(),
            sample_rate: 48_000,
            time_sig_num: 5,
            target_lufs: -14.0,
            zoom: 1.5,
            snap_to_grid: false,
            layer_names: vec!["Kick".into(), "Bass".into()],
            take: Some(TakeState {
                armed: vec![0, 1],
                start_pulses: 1234,
                bpm: 120.0,
                time_sig_num: 5,
                take_user_offset_units: -0.25,
            }),
        };
        save(&dir, &s).unwrap();
        let back = load(&dir).unwrap();
        assert_eq!(back.session_name, "demo");
        assert_eq!(back.time_sig_num, 5);
        assert_eq!(back.layer_names, vec!["Kick", "Bass"]);
        let t = back.take.unwrap();
        assert_eq!(t.armed, vec![0, 1]);
        assert_eq!(t.start_pulses, 1234);
        assert!((t.take_user_offset_units - (-0.25)).abs() < 1e-6);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_without_take() {
        let dir = std::env::temp_dir()
            .join(format!("gatherer-session-test-no-take-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let s = SessionState {
            session_name: "fresh".into(),
            sample_rate: 48_000,
            time_sig_num: 4,
            target_lufs: -23.0,
            zoom: 1.0,
            snap_to_grid: true,
            layer_names: vec![],
            take: None,
        };
        save(&dir, &s).unwrap();
        let back = load(&dir).unwrap();
        assert!(back.take.is_none());
        assert!(back.snap_to_grid);
        let _ = fs::remove_dir_all(&dir);
    }
}

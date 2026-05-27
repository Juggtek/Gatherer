//! Stems export — writes per-source 32-bit float WAVs to disk, both as
//! recorded (original) and gain-adjusted to a target LUFS (normalized).

use crate::measurement::{normalization_gain, LufsMeasurement};
use crate::playback::PlaybackData;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct ExportResult {
    pub dir: PathBuf,
    #[allow(dead_code)] // used in tests + future UI status string
    pub source_count: usize,
}

/// Write `unnormalized/` and `normalized/` subfolders under the session
/// root (sibling of `recording/`). Filenames come from `layer_names[src_idx]`
/// when non-empty (sanitized for the filesystem), else `source-NN.wav`.
///
/// `source_indices` is parallel to `data.sources` — `data.sources[i]` is
/// the audio for `source_indices[i]` (the user-side slot index).
pub fn export_stems(
    data: &PlaybackData,
    sample_rate: u32,
    session_root: &Path,
    source_indices: &[usize],
    layer_names: &[String],
    measurements: &HashMap<usize, LufsMeasurement>,
    target_lufs: f64,
) -> Result<ExportResult, String> {
    let unnorm_dir = session_root.join("unnormalized");
    let norm_dir = session_root.join("normalized");
    std::fs::create_dir_all(&unnorm_dir)
        .map_err(|e| format!("create {}: {e}", unnorm_dir.display()))?;
    std::fs::create_dir_all(&norm_dir)
        .map_err(|e| format!("create {}: {e}", norm_dir.display()))?;

    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };

    let mut written = 0;
    for (i, &src_idx) in source_indices.iter().enumerate() {
        let Some(samples) = data.sources.get(i) else {
            continue;
        };
        let name = layer_filename(layer_names, src_idx);
        let unnorm_path = unnorm_dir.join(format!("{name}.wav"));
        write_wav(&unnorm_path, spec, samples, 1.0)?;

        let gain = measurements
            .get(&src_idx)
            .map(|m| normalization_gain(m.integrated, target_lufs))
            .unwrap_or(1.0);
        let norm_path = norm_dir.join(format!("{name}.wav"));
        write_wav(&norm_path, spec, samples, gain)?;
        written += 1;
    }

    Ok(ExportResult {
        dir: session_root.to_path_buf(),
        source_count: written,
    })
}

fn layer_filename(layer_names: &[String], src_idx: usize) -> String {
    let raw = layer_names
        .get(src_idx)
        .map(|s| s.trim())
        .unwrap_or("");
    if raw.is_empty() {
        format!("source-{:02}", src_idx + 1)
    } else {
        sanitize(raw)
    }
}

/// Conservative filesystem-safe name: keep alphanumerics, space, dash,
/// underscore, dot; collapse everything else to `_`.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn write_wav(
    path: &Path,
    spec: hound::WavSpec,
    samples: &Arc<Vec<f32>>,
    gain: f32,
) -> Result<(), String> {
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    for &s in samples.iter() {
        writer
            .write_sample(s * gain)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    writer
        .finalize()
        .map_err(|e| format!("finalize {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::measurement::measure;

    #[test]
    fn export_round_trip_writes_two_flavors() {
        // 0.5 s of stereo sine at moderate level.
        let sr: u32 = 48_000;
        let n: usize = sr as usize / 2;
        let amp = 0.5f32;
        let mut s = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let v = amp * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            s.push(v);
            s.push(v);
        }
        let samples = Arc::new(s);
        let measurement = measure(&samples, 2, sr);
        let mut meas_map = HashMap::new();
        meas_map.insert(0, measurement);

        let data = PlaybackData {
            sources: vec![samples],
            len_frames: n,
            start_pulses: 0,
            bpm: 120.0,
            time_sig_num: 4,
            armed: vec![0],
        };

        let session_root = std::env::temp_dir()
            .join(format!("gatherer-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&session_root).unwrap();

        // Named layer "Vocals" + an unnamed slot to exercise the fallback.
        let layer_names = vec!["Vocals".to_string()];
        let result = export_stems(
            &data,
            sr,
            &session_root,
            &[0],
            &layer_names,
            &meas_map,
            -14.0,
        )
        .unwrap();
        let unnorm = result.dir.join("unnormalized").join("Vocals.wav");
        let norm = result.dir.join("normalized").join("Vocals.wav");
        assert!(unnorm.exists());
        assert!(norm.exists());
        assert_eq!(result.source_count, 1);

        let _ = std::fs::remove_dir_all(&session_root);
    }

    #[test]
    fn sanitize_drops_unsafe_chars() {
        assert_eq!(sanitize("Vocals"), "Vocals");
        assert_eq!(sanitize("Vocals/Lead"), "Vocals_Lead");
        assert_eq!(sanitize("kick: 1"), "kick_ 1"); // space is allowed
        assert_eq!(sanitize("a*b?c"), "a_b_c");
    }

    #[test]
    fn layer_filename_falls_back_when_blank() {
        assert_eq!(layer_filename(&[], 3), "source-04");
        assert_eq!(layer_filename(&["   ".to_string()], 0), "source-01");
        assert_eq!(layer_filename(&["Bass".to_string()], 0), "Bass");
    }
}

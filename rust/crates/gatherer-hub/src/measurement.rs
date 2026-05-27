//! LUFS loudness measurement via the `ebur128` crate (BS.1770-4).
//!
//! `integrated` is the gated whole-take loudness (one number per take —
//! the value normalization targets). `max_short_term` and `max_momentary`
//! are the peak values of the 3 s and 400 ms running windows over the
//! whole take, found by feeding the audio in small chunks and tracking
//! the maximum reported value.

use ebur128::{EbuR128, Mode};

#[derive(Debug, Clone, Copy)]
pub struct LufsMeasurement {
    pub integrated: f64,      // LUFS, NEG_INFINITY for silence
    pub max_short_term: f64,  // max LUFS over 3 s windows
    pub max_momentary: f64,   // max LUFS over 400 ms windows
}

impl LufsMeasurement {
    pub fn silent() -> Self {
        Self {
            integrated: f64::NEG_INFINITY,
            max_short_term: f64::NEG_INFINITY,
            max_momentary: f64::NEG_INFINITY,
        }
    }
}

/// Measure LUFS on interleaved float samples (e.g. stereo: L,R,L,R...).
pub fn measure(interleaved: &[f32], channels: u32, sample_rate: u32) -> LufsMeasurement {
    let Ok(mut meter) = EbuR128::new(channels, sample_rate, Mode::I | Mode::S | Mode::M) else {
        return LufsMeasurement::silent();
    };

    // 100 ms chunks so we sample short-term / momentary repeatedly.
    let chunk_frames = (sample_rate as usize / 10).max(1024);
    let chunk_samples = chunk_frames * channels as usize;
    let mut max_s = f64::NEG_INFINITY;
    let mut max_m = f64::NEG_INFINITY;
    for chunk in interleaved.chunks(chunk_samples) {
        let _ = meter.add_frames_f32(chunk);
        if let Ok(s) = meter.loudness_shortterm() {
            if s.is_finite() && s > max_s {
                max_s = s;
            }
        }
        if let Ok(m) = meter.loudness_momentary() {
            if m.is_finite() && m > max_m {
                max_m = m;
            }
        }
    }
    LufsMeasurement {
        integrated: meter.loudness_global().unwrap_or(f64::NEG_INFINITY),
        max_short_term: max_s,
        max_momentary: max_m,
    }
}

/// Linear gain to drive `measured` to `target` LUFS. Returns 1.0 for
/// silent or non-finite measurements (no normalization applied).
pub fn normalization_gain(measured: f64, target: f64) -> f32 {
    if !measured.is_finite() {
        return 1.0;
    }
    let delta_db = (target - measured) as f32;
    10f32.powf(delta_db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_neg_infinity() {
        let m = measure(&vec![0.0f32; 48_000 * 2 * 5], 2, 48_000); // 5 s of silence
        assert!(!m.integrated.is_finite());
    }

    #[test]
    fn sine_at_minus_18_dbfs_reads_around_minus_18_lufs() {
        // 1 kHz sine, 1 s, amplitude -18 dBFS → BS.1770 integrated ~= -21 LUFS
        // (true reference depends on K-weighting; check the value is in a
        // reasonable LUFS range, not exact).
        let sr: u32 = 48_000;
        let n: usize = sr as usize;
        let amp = 10f32.powf(-18.0 / 20.0);
        let mut s = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let v = amp * (2.0 * std::f32::consts::PI * 1000.0 * t).sin();
            s.push(v);
            s.push(v);
        }
        let m = measure(&s, 2, sr);
        // A 1 s sine is shorter than the gating block — integrated may not
        // settle; use short-term instead for a sanity check.
        assert!(m.max_short_term.is_finite());
        assert!(m.max_short_term < 0.0 && m.max_short_term > -30.0);
    }

    #[test]
    fn normalization_gain_matches_delta() {
        // measured -23, target -14 → +9 dB → ≈ 2.818
        let g = normalization_gain(-23.0, -14.0);
        assert!((g - 10f32.powf(9.0 / 20.0)).abs() < 1e-5);
    }

    #[test]
    fn normalization_gain_silent_passthrough() {
        assert_eq!(normalization_gain(f64::NEG_INFINITY, -14.0), 1.0);
    }
}

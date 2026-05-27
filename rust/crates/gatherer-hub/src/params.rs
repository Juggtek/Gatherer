//! Shared parameter atomics between the audio thread and the UI thread.
//!
//! Plain Rust atomics, the FIELD pattern: the audio callback reads via
//! `Relaxed` loads; the UI writes via `Relaxed` stores. No event queue,
//! no smoother targets to refresh — atomics ARE the bridge.

use atomic_float::AtomicF32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const GAIN_DB_MIN: f32 = -60.0;
pub const GAIN_DB_MAX: f32 = 12.0;

/// One gathered source = one stereo input-channel pair on the capture
/// device. Mix controls live here; the audio thread reads them per block
/// and the UI writes them on user interaction. `peak_l/peak_r` flow the
/// other way: audio writes, UI reads on its meter tick.
#[derive(Debug)]
pub struct SourceParams {
    /// Linear gain (UI exposes dB; stored linear so the audio thread
    /// avoids `powf` per block).
    pub gain: AtomicF32,
    pub muted: AtomicBool,
    pub soloed: AtomicBool,
    /// Polarity invert — the M1 polarity-null acceptance test needs it.
    pub invert: AtomicBool,
    /// Post-gain peak amplitudes for the stereo meter, written each block.
    pub peak_l: AtomicF32,
    pub peak_r: AtomicF32,
}

impl SourceParams {
    pub fn new() -> Self {
        Self {
            gain: AtomicF32::new(1.0),
            muted: AtomicBool::new(false),
            soloed: AtomicBool::new(false),
            invert: AtomicBool::new(false),
            peak_l: AtomicF32::new(0.0),
            peak_r: AtomicF32::new(0.0),
        }
    }

    #[inline]
    pub fn load_gain(&self) -> f32 {
        self.gain.load(Ordering::Relaxed)
    }
    pub fn store_gain_db(&self, db: f32) {
        self.gain
            .store(db_to_linear(db.clamp(GAIN_DB_MIN, GAIN_DB_MAX)), Ordering::Relaxed);
    }
    pub fn gain_db(&self) -> f32 {
        linear_to_db(self.load_gain())
    }

    #[inline]
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
    #[inline]
    pub fn is_soloed(&self) -> bool {
        self.soloed.load(Ordering::Relaxed)
    }
    #[inline]
    pub fn is_inverted(&self) -> bool {
        self.invert.load(Ordering::Relaxed)
    }

    /// Read+clear the meter peaks (UI calls this on its tick).
    pub fn take_peaks(&self) -> (f32, f32) {
        (
            self.peak_l.swap(0.0, Ordering::Relaxed),
            self.peak_r.swap(0.0, Ordering::Relaxed),
        )
    }
}

impl Default for SourceParams {
    fn default() -> Self {
        Self::new()
    }
}

/// Whole-hub shared parameters: a master gain plus one `SourceParams`
/// per gathered stereo source. Cloned (Arc) into the audio callback.
#[derive(Debug, Clone)]
pub struct HubParams {
    pub master_gain: Arc<AtomicF32>,
    pub sources: Arc<Vec<Arc<SourceParams>>>,
    pub master_peak_l: Arc<AtomicF32>,
    pub master_peak_r: Arc<AtomicF32>,
    /// Per-source playback normalization gain (linear). Updated by the UI
    /// from `target_lufs - integrated_lufs`; the audio thread reads it on
    /// each block. Default 1.0 (no normalization).
    pub normalization_gains: Arc<Vec<AtomicF32>>,
}

impl HubParams {
    /// `num_sources` stereo sources (= device_input_channels / 2).
    pub fn new(num_sources: usize) -> Self {
        let sources = (0..num_sources)
            .map(|_| Arc::new(SourceParams::new()))
            .collect::<Vec<_>>();
        let normalization_gains = (0..num_sources)
            .map(|_| AtomicF32::new(1.0))
            .collect::<Vec<_>>();
        Self {
            master_gain: Arc::new(AtomicF32::new(1.0)),
            sources: Arc::new(sources),
            master_peak_l: Arc::new(AtomicF32::new(0.0)),
            master_peak_r: Arc::new(AtomicF32::new(0.0)),
            normalization_gains: Arc::new(normalization_gains),
        }
    }

    #[inline]
    pub fn load_master_gain(&self) -> f32 {
        self.master_gain.load(Ordering::Relaxed)
    }

    /// True if any source is soloed — gates non-soloed sources.
    #[allow(dead_code)] // used in tests; UI will use it to dim non-soloed rows
    pub fn any_soloed(&self) -> bool {
        self.sources.iter().any(|s| s.is_soloed())
    }
}

#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[inline]
pub fn linear_to_db(linear: f32) -> f32 {
    if linear < 1e-9 {
        GAIN_DB_MIN
    } else {
        20.0 * linear.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn db_linear_roundtrip() {
        for db in [-48.0, -12.0, -6.0, 0.0, 6.0] {
            let back = linear_to_db(db_to_linear(db));
            assert!((back - db).abs() < 1e-3, "{db} -> {back}");
        }
        assert!((db_to_linear(0.0) - 1.0).abs() < 1e-6); // 0 dB == unity
    }

    #[test]
    fn store_gain_db_clamps_to_range() {
        let sp = SourceParams::new();
        sp.store_gain_db(999.0);
        assert!((sp.load_gain() - db_to_linear(GAIN_DB_MAX)).abs() < 1e-6);
        sp.store_gain_db(-999.0);
        assert!((sp.load_gain() - db_to_linear(GAIN_DB_MIN)).abs() < 1e-6);
    }

    #[test]
    fn take_peaks_clears() {
        let sp = SourceParams::new();
        sp.peak_l.store(0.5, Ordering::Relaxed);
        sp.peak_r.store(0.25, Ordering::Relaxed);
        assert_eq!(sp.take_peaks(), (0.5, 0.25));
        assert_eq!(sp.take_peaks(), (0.0, 0.0)); // cleared by the swap
    }

    #[test]
    fn any_soloed_reflects_sources() {
        let p = HubParams::new(3);
        assert!(!p.any_soloed());
        p.sources[1].soloed.store(true, Ordering::Relaxed);
        assert!(p.any_soloed());
    }
}

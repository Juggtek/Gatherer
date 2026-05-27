//! Adaptive mixer scaffold.
//!
//! The adaptive mixer runs on the UI tick (~30 Hz). It reads per-source
//! state from `HubParams` (peaks, current gain) and writes new gain
//! values back via `SourceParams::store_gain_db` — exactly what the
//! per-source volume slider does, so the visible faders move along with
//! the algorithm.
//!
//! Position in the signal chain (per playback frame):
//!
//!   raw take audio → ×normalization_gain → ×mixer_gain × invert
//!     → sum across sources → ×master → output
//!                  ↑
//!                  └─ adaptive mixer writes `mixer_gain` (i.e. the slider)
//!
//! The actual control logic is supplied separately (translated from a
//! user-provided Max patch). Until that lands, `step` is a no-op so the
//! user's manual fader values stick.

use crate::params::HubParams;
#[allow(unused_imports)] // logic placeholder references it in comments
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Read-only snapshot for one tick.
#[allow(dead_code)] // fields wired but unused until the Max-patch logic lands
#[derive(Debug, Clone, Copy)]
pub struct MixerContext {
    /// Wall-clock seconds since the adaptive mixer was enabled.
    pub elapsed_seconds: f32,
    /// Seconds since the previous `step()` call (UI tick delta, ~33 ms).
    pub dt_seconds: f32,
    /// True if the loaded take is currently playing back.
    pub playing: bool,
}

pub struct AdaptiveMixer {
    enabled: bool,
    started_at: Option<Instant>,
    last_step_at: Option<Instant>,
}

impl AdaptiveMixer {
    pub fn new() -> Self {
        Self {
            enabled: false,
            started_at: None,
            last_step_at: None,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if on {
            let now = Instant::now();
            self.started_at = Some(now);
            self.last_step_at = Some(now);
        } else {
            self.started_at = None;
            self.last_step_at = None;
        }
    }

    pub fn elapsed_seconds(&self) -> f32 {
        self.started_at
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0)
    }

    /// One step. Called from the UI tick handler.
    ///
    /// Reads from `params.sources[i]` (per-source peaks via
    /// `peak_l.load`/`peak_r.load` — **not** `take_peaks()` which would
    /// steal the peak from the meter UI) and writes new gain via
    /// `params.sources[i].store_gain_db(db)`.
    pub fn step(&mut self, params: &HubParams, playing: bool) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let dt = self
            .last_step_at
            .map(|t| (now - t).as_secs_f32())
            .unwrap_or(0.033);
        self.last_step_at = Some(now);
        let _ctx = MixerContext {
            elapsed_seconds: self.elapsed_seconds(),
            dt_seconds: dt,
            playing,
        };

        // ─── ADAPTIVE LOGIC GOES HERE ───────────────────────────────
        // Shape, per source `i in 0..params.sources.len()`:
        //
        //   let sp = &params.sources[i];
        //   let peak_l = sp.peak_l.load(Ordering::Relaxed);
        //   let peak_r = sp.peak_r.load(Ordering::Relaxed);
        //   let current_db = sp.gain_db();
        //   let new_db = /* compute from peaks, ctx, history, ... */;
        //   sp.store_gain_db(new_db);
        //
        // Replace with the translation of the user's Max patch.
        // ────────────────────────────────────────────────────────────
        let _ = params;
    }
}

impl Default for AdaptiveMixer {
    fn default() -> Self {
        Self::new()
    }
}

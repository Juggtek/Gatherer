//! Adaptive mixer — Rust port of the Max patch
//! `Adaptive-Mixer_1.0.0_mac.maxpat` (siblings: `Layer_patch.maxpat`,
//! `Formula_plotter.maxpat`).
//!
//! Control loop (runs on the UI tick, ~30 Hz, while `enabled`):
//!
//!   for each of 8 slots i:
//!     params  = source_params[i]                       ; 6 floats
//!     raw_i   = curve(params.formula, intensity,       ; selects one of 9 curves
//!                     steepness, deviation,
//!                     original_level,                  ; $f4 = vol
//!                     mood_weight[mood][i])            ; $f5 = M1 mask
//!     raw_i   = raw_i.clamp(minimum, maximum)          ; per-source clip bounds
//!     power_i = raw_i² × balancer_mask[mood][i]        ; M2 mask = power weight
//!   total       = Σ power_i
//!   power_sum   = sqrt(total)                          ; UI's POWER SUM
//!   target      = target_curve_value()                 ; per-mode value (v1 = scalar)
//!   factor      = if activate_target_curve {
//!                   target / (power_sum + 0.01)
//!                 } else { 1.0 }                       ; UI's FACTOR
//!   for each slot i:
//!     final_i   = raw_i × factor                       ; mood already baked into raw
//!     smoothed  = slew_i.step(final_i, smooth_ms, dt)
//!     params.sources[i].gain = smoothed                ; what the slider writes
//!
//! Position in the signal chain (per playback frame in `audio.rs`):
//!
//!   raw take audio → ×normalization_gain (BS.1770, in `measurement`)
//!                  → ×mixer_gain × invert (mixer_gain set by us above)
//!                  → sum across sources → ×master → output
//!
//! See ADAPTIVE-MIXER.md for the formulas and the template format.

use crate::params::HubParams;
use std::sync::atomic::Ordering;
use std::time::Instant;

pub const SLOT_COUNT: usize = 8;
#[allow(dead_code)] // documented constant; the curve() match is the source of truth
pub const FORMULA_COUNT: usize = 9; // `switch 9` in the patch

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    Dark = 0,
    Neutral = 1,
    Bright = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Music = 0,
    Locals = 1,
    Globals = 2,
    Combat = 3,
}

impl Mood {
    #[allow(dead_code)] // useful for iterating in templates / future UI
    pub const ALL: [Mood; 3] = [Mood::Dark, Mood::Neutral, Mood::Bright];
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Mood::Dark => "Dark",
            Mood::Neutral => "Neutral",
            Mood::Bright => "Bright",
        }
    }
}

impl Mode {
    #[allow(dead_code)]
    pub const ALL: [Mode; 4] = [Mode::Music, Mode::Locals, Mode::Globals, Mode::Combat];
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Mode::Music => "Music",
            Mode::Locals => "Locals",
            Mode::Globals => "Globals",
            Mode::Combat => "Combat",
        }
    }
}

/// Per-slot curve parameters (the six per-source values in the patch).
#[derive(Debug, Clone, Copy)]
pub struct SlotParams {
    pub steepness: f32,      // $f2
    pub deviation: f32,      // $f3
    pub maximum: f32,        // upper bound of post-curve clip
    pub minimum: f32,        // lower bound of post-curve clip
    pub original_level: f32, // $f4 (Layer_patch's `vol` inlet)
    pub formula: u8,         // 1..=9
}

/// One of the five continuous fields on `SlotParams` (used by UI messages
/// to update a slot or the target curve without exploding into 5 variants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotField {
    Steepness,
    Deviation,
    Maximum,
    Minimum,
    Original,
}

impl SlotParams {
    pub fn set_field(&mut self, field: SlotField, value: f32) {
        let v = value.clamp(0.0, 2.0);
        match field {
            SlotField::Steepness => self.steepness = v.min(1.0),
            SlotField::Deviation => self.deviation = v.min(1.0),
            SlotField::Maximum => self.maximum = v.min(2.0),
            SlotField::Minimum => self.minimum = v.min(1.0),
            SlotField::Original => self.original_level = v.min(1.0),
        }
    }

    /// Evaluate the slot's curve at `intensity` with per-source `mood` mask
    /// (Layer_patch's `mood` inlet, the M1 weight). Clipped to
    /// `[minimum, maximum]`. Uses `max(min).min(max)` so a bad
    /// `minimum > maximum` collapses to `maximum` instead of panicking.
    pub fn eval(self, intensity: f32, mood: f32) -> f32 {
        let v = curve(
            self.formula,
            intensity,
            self.steepness,
            self.deviation,
            self.original_level,
            mood,
        );
        v.max(self.minimum).min(self.maximum)
    }
}

impl Default for SlotParams {
    /// Music_template defaults — every source the same: Formula 1 from the
    /// template's repeating block.
    fn default() -> Self {
        Self {
            steepness: 0.150,
            deviation: 0.090,
            maximum: 1.000,
            minimum: 0.000,
            original_level: 0.310,
            formula: 1,
        }
    }
}

/// Per-source preset bank — verbatim from `Adaptive-Mixer_1.0.0_mac.maxpat`'s
/// 9-column "IMPORT" matrix (the 6×9 grid of messages at x≈2261..2513,
/// y≈566..675, fed into `join 6` packers). One preset per formula 1..9,
/// each a known-good shape that exercises that formula well.
pub const SOURCE_PRESETS: [(&str, SlotParams); 9] = [
    ("F1", SlotParams { steepness: 0.15, deviation: 0.09, maximum: 1.0, minimum: 0.0, original_level: 0.31, formula: 1 }),
    ("F2", SlotParams { steepness: 0.57, deviation: 0.00, maximum: 1.0, minimum: 0.0, original_level: 0.15, formula: 2 }),
    ("F3", SlotParams { steepness: 0.32, deviation: 0.23, maximum: 1.0, minimum: 0.0, original_level: 0.70, formula: 3 }),
    ("F4", SlotParams { steepness: 0.61, deviation: 0.19, maximum: 1.0, minimum: 0.0, original_level: 1.00, formula: 4 }),
    ("F5", SlotParams { steepness: 0.40, deviation: 0.37, maximum: 1.0, minimum: 0.0, original_level: 1.00, formula: 5 }),
    ("F6", SlotParams { steepness: 0.47, deviation: 0.45, maximum: 1.0, minimum: 0.0, original_level: 1.00, formula: 6 }),
    ("F7", SlotParams { steepness: 0.20, deviation: 0.20, maximum: 1.0, minimum: 0.0, original_level: 0.50, formula: 7 }),
    ("F8", SlotParams { steepness: 0.30, deviation: 0.30, maximum: 1.0, minimum: 0.0, original_level: 0.20, formula: 8 }),
    ("F9", SlotParams { steepness: 0.40, deviation: 0.50, maximum: 1.0, minimum: 0.0, original_level: 0.80, formula: 9 }),
];

/// Per-mode target-curve presets — verbatim from the "Select Target Curve"
/// matrix in `Adaptive-Mixer_1.0.0_mac.maxpat` (4 columns × 6 rows of
/// message boxes at x≈913/948/981/1015, y≈1665..1781). Indexed by
/// `Mode as usize` — Music, Locals, Globals, Combat.
pub const TARGET_PRESETS: [SlotParams; 4] = [
    // Music
    SlotParams { steepness: 0.20, deviation: 0.50, maximum: 1.0, minimum: 0.0, original_level: 0.50, formula: 7 },
    // Locals
    SlotParams { steepness: 0.20, deviation: 0.35, maximum: 1.0, minimum: 0.0, original_level: 0.51, formula: 7 },
    // Globals
    SlotParams { steepness: 0.20, deviation: 0.25, maximum: 1.0, minimum: 0.0, original_level: 0.51, formula: 7 },
    // Combat
    SlotParams { steepness: 0.10, deviation: 0.50, maximum: 1.0, minimum: 0.0, original_level: 0.52, formula: 7 },
];

#[derive(Debug, Default, Clone, Copy)]
struct Slew {
    current: f32,
}

impl Slew {
    fn step(&mut self, target: f32, smooth_ms: f32, dt_seconds: f32) -> f32 {
        let smooth_s = (smooth_ms / 1000.0).max(0.001);
        // Linear ramp: cover (target - current) over smooth_s seconds.
        let max_delta = (dt_seconds / smooth_s) * (target - self.current).abs();
        let delta = (target - self.current).clamp(-max_delta, max_delta);
        self.current += delta;
        self.current
    }
}

pub struct AdaptiveMixer {
    pub enabled: bool,
    pub intensity: f32, // 0..1, manual slider
    pub mood: Mood,
    pub mode: Mode,
    pub smooth_ms: f32, // ramp time
    pub activate_target_curve: bool,

    pub slot_params: [SlotParams; SLOT_COUNT],

    /// `mood_weight[mood][slot]` — M1 mask in the patch (`ML0..ML7`,
    /// per-mood). Fed into the curve as `$f5`, so it scales the raw
    /// formula before clipping (= each mood's "voice" for each source).
    pub mood_weight: [[f32; SLOT_COUNT]; 3],
    /// `balancer_mask[mood][slot]` — M2 mask in the patch
    /// (`M2L0..M2L7`, per-mood). Used as the power-sum weight
    /// (`raw² × balancer`), i.e. how much each source contributes to the
    /// headroom budget that the target-curve factor normalizes against.
    pub balancer_mask: [[f32; SLOT_COUNT]; 3],

    /// Per-mode target curve, evaluated at `intensity` each tick. The
    /// loudness curve to which the mix is normalized. One `SlotParams`
    /// per mode (Music/Locals/Globals/Combat).
    pub target_curve: [SlotParams; 4],

    // Last computed values exposed for the UI.
    pub last_power_sum: f32,
    pub last_factor: f32,
    pub last_target: f32,

    started_at: Option<Instant>,
    last_step_at: Option<Instant>,
    slews: [Slew; SLOT_COUNT],
}

impl AdaptiveMixer {
    pub fn new() -> Self {
        Self {
            enabled: false,
            intensity: 0.0,
            mood: Mood::Neutral,
            mode: Mode::Music,
            smooth_ms: 200.0,
            activate_target_curve: true,
            slot_params: [SlotParams::default(); SLOT_COUNT],
            mood_weight: [[1.0; SLOT_COUNT]; 3],
            balancer_mask: [[1.0; SLOT_COUNT]; 3],
            // Per-mode target curves seeded from the Max patch's
            // "Select Target Curve" matrix (see `TARGET_PRESETS`).
            target_curve: TARGET_PRESETS,
            last_power_sum: 0.0,
            last_factor: 1.0,
            last_target: 1.0,
            started_at: None,
            last_step_at: None,
            slews: [Slew::default(); SLOT_COUNT],
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        let now = Instant::now();
        if on {
            self.started_at = Some(now);
            self.last_step_at = Some(now);
        } else {
            self.started_at = None;
            self.last_step_at = None;
        }
    }
    #[allow(dead_code)] // shown in the future UI status line
    pub fn elapsed_seconds(&self) -> f32 {
        self.started_at
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0)
    }

    /// Evaluate the target curve for `mode` at `intensity`. The target
    /// curve has no per-source mood mask, so we pass `mood = 1.0`
    /// (identity for the `$f5` factor in every formula).
    pub fn target_at(&self, mode: Mode, intensity: f32) -> f32 {
        self.target_curve[mode as usize].eval(intensity, 1.0)
    }

    /// Sample the raw + compensated curve for one source slot across
    /// `intensity in 0..1` (`n` samples). The compensated curve includes
    /// the target-curve normalization (so the user sees what their slider
    /// actually does, not just the raw formula).
    pub fn slot_curves(&self, slot: usize, n: usize) -> (Vec<f32>, Vec<f32>) {
        let mut raws_all = [0.0f32; SLOT_COUNT];
        let mood = self.mood as usize;
        let mut raw_pts = Vec::with_capacity(n);
        let mut comp_pts = Vec::with_capacity(n);
        let denom = (n.saturating_sub(1).max(1)) as f32;
        for k in 0..n {
            let intensity = k as f32 / denom;
            for s in 0..SLOT_COUNT {
                raws_all[s] = self.slot_params[s].eval(intensity, self.mood_weight[mood][s]);
            }
            let mut total = 0.0f32;
            for s in 0..SLOT_COUNT {
                total += raws_all[s] * raws_all[s] * self.balancer_mask[mood][s];
            }
            let power_sum = total.sqrt();
            let target = self.target_at(self.mode, intensity);
            let factor = if self.activate_target_curve {
                target / (power_sum + 0.01)
            } else {
                1.0
            };
            let raw = raws_all.get(slot).copied().unwrap_or(0.0);
            raw_pts.push(raw);
            comp_pts.push(raw * factor);
        }
        (raw_pts, comp_pts)
    }

    /// Sample the target curve (current mode) across intensity 0..1.
    pub fn target_curve_points(&self, n: usize) -> Vec<f32> {
        let denom = (n.saturating_sub(1).max(1)) as f32;
        (0..n)
            .map(|k| self.target_at(self.mode, k as f32 / denom))
            .collect()
    }

    /// One step. Runs from the UI's Tick handler.
    pub fn step(&mut self, params: &HubParams) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let dt = self
            .last_step_at
            .map(|t| (now - t).as_secs_f32().max(0.001))
            .unwrap_or(0.033);
        self.last_step_at = Some(now);

        let mood = self.mood as usize;

        // Step 1: raw gains via the per-slot formula. mood mask is fed
        // into the curve as $f5; result is clipped to [minimum, maximum].
        let mut raw = [0.0f32; SLOT_COUNT];
        for i in 0..SLOT_COUNT {
            raw[i] = self.slot_params[i].eval(self.intensity, self.mood_weight[mood][i]);
        }

        // Step 2: power = raw² × balancer_mask (M2). Sum, sqrt for the aggregate.
        let mut total_power = 0.0f32;
        for i in 0..SLOT_COUNT {
            let w = self.balancer_mask[mood][i];
            total_power += raw[i] * raw[i] * w;
        }
        let power_sum = total_power.sqrt();

        // Step 3: target curve evaluated at current intensity (per-mode formula).
        let target = self.target_at(self.mode, self.intensity);

        // Step 4: target-curve normalization factor.
        let factor = if self.activate_target_curve {
            target / (power_sum + 0.01)
        } else {
            1.0
        };

        // Step 5: per-slot final gain → slew → publish into the mixer
        // slider. No post-factor balancer multiply — the mask is already
        // baked into `raw` (via `$f5`) and into `power_sum` (via M2).
        let max_slots = params.sources.len().min(SLOT_COUNT);
        for i in 0..max_slots {
            let final_gain = raw[i] * factor;
            let smoothed = self.slews[i].step(final_gain, self.smooth_ms, dt);
            params.sources[i]
                .gain
                .store(smoothed.max(0.0), Ordering::Relaxed);
        }

        self.last_power_sum = power_sum;
        self.last_factor = factor;
        self.last_target = target;
    }
}

impl Default for AdaptiveMixer {
    fn default() -> Self {
        Self::new()
    }
}

/// The 9 curve presets from `Layer_patch.maxpat` (switch inlets 1..=9).
/// `i = intensity ($f1)`, `s = steepness ($f2)`, `d = deviation ($f3)`,
/// `vol = original_level ($f4)`, `mood = per-source M1 mask value ($f5)`.
/// All ported verbatim from the patch. (The Layer's `min`, `max`,
/// `formula` inlets pick the formula and clip its output — not part of
/// the math here; see `SlotParams::eval`.)
pub fn curve(formula: u8, i: f32, s: f32, d: f32, vol: f32, mood: f32) -> f32 {
    let i2 = i * i;
    let i3 = i2 * i;
    match formula {
        1 => {
            // ($f5·$f4 + $f5·$f3·$f1·(2$f1−1))·pow(10$f2+1, 2$f1−1)
            //   − pow(1−$f1, 3)·$f5·$f4/(10$f2+1)
            (mood * vol + mood * d * i * (2.0 * i - 1.0)) * (10.0 * s + 1.0).powf(2.0 * i - 1.0)
                - (1.0 - i).powi(3) * mood * vol / (10.0 * s + 1.0)
        }
        2 => {
            // $f5·$f4·pow(10$f2+1, 1−2$f1) + $f5·$f3·$f1·(2$f1−1)
            //   − $f5·$f4·pow($f1, 3)/(10$f2+1)
            mood * vol * (10.0 * s + 1.0).powf(1.0 - 2.0 * i) + mood * d * i * (2.0 * i - 1.0)
                - mood * vol * i3 / (10.0 * s + 1.0)
        }
        3 => {
            // (1+2$f3)·$f5·$f4·pow($f1, 10$f2+0.2) − $f5·$f4·pow($f3, 10$f2+0.2)
            (1.0 + 2.0 * d) * mood * vol * i.powf(10.0 * s + 0.2)
                - mood * vol * d.powf(10.0 * s + 0.2)
        }
        4 => {
            // ($f5·$f4 / pow(1+$f1−$f3, 10$f2−$f3)) − $f5·$f4·$f3·$f1
            mood * vol / (1.0 + i - d).powf(10.0 * s - d) - mood * vol * d * i
        }
        5 => {
            // $f5·$f4 / (1+10·exp(25$f3$f2 − 30$f2$f1))
            //   − $f5·$f4·pow(1−$f1, 3) / (100$f3$f2 + 10$f5$f4 + 1)
            mood * vol / (1.0 + 10.0 * (25.0 * d * s - 30.0 * s * i).exp())
                - mood * vol * (1.0 - i).powi(3) / (100.0 * d * s + 10.0 * mood * vol + 1.0)
        }
        6 => {
            // $f5·$f4 − $f5·$f4/(1+10·exp(25$f3$f2−30$f2$f1))
            //   − $f5·$f4·pow($f1, 3) / (20($f2−$f3$f2) + 4$f2 + 4.1 − 4$f5·pow($f4, 2))
            mood * vol - mood * vol / (1.0 + 10.0 * (25.0 * d * s - 30.0 * s * i).exp())
                - mood * vol * i3
                    / (20.0 * (s - d * s) + 4.0 * s + 4.1 - 4.0 * mood * vol * vol)
        }
        7 => {
            // 2·$f5·(1−$f3)·$f4·pow($f1, 10$f2) + $f5·$f3
            2.0 * mood * (1.0 - d) * vol * i.powf(10.0 * s) + mood * d
        }
        8 => {
            // $f5·(1.02−$f4) / (1+10·exp(25$f3$f2 − 30$f2$f1)) + $f4
            mood * (1.02 - vol) / (1.0 + 10.0 * (25.0 * d * s - 30.0 * s * i).exp()) + vol
        }
        9 => {
            // 2·pow($f5·$f4, 2)·exp(−pow($f1−$f3, 2)/(1/1000 + (0.9 − 2$f4/3)·pow($f2, 2)))
            //   − (1.02 − $f5·$f4) / (50·pow(1.2 − $f5·$f4, 2))
            let denom = 1.0 / 1000.0 + (0.9 - 2.0 * vol / 3.0) * s * s;
            2.0 * (mood * vol).powi(2) * (-((i - d).powi(2) / denom)).exp()
                - (1.02 - mood * vol) / (50.0 * (1.2 - mood * vol).powi(2))
        }
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_returns_zero_for_unknown_formula() {
        // args: (formula, intensity, s, d, vol, mood)
        assert_eq!(curve(0, 0.5, 0.5, 0.5, 1.0, 1.0), 0.0);
        assert_eq!(curve(99, 0.5, 0.5, 0.5, 1.0, 1.0), 0.0);
    }

    #[test]
    fn curve_7_is_zero_at_intensity_zero_with_no_offset() {
        // F7 = 2·mood·(1−d)·vol·i^(10s) + mood·d. At i=0 the first term
        // vanishes (positive exponent), and d=0 zeroes the offset too.
        let v = curve(7, 0.0, 0.5, 0.0, 1.0, 1.0);
        assert!(v.abs() < 1e-6, "F7(i=0, d=0) should be 0, got {v}");
    }

    #[test]
    fn curve_7_grows_with_intensity() {
        let lo = curve(7, 0.1, 0.5, 0.0, 1.0, 1.0);
        let hi = curve(7, 0.9, 0.5, 0.0, 1.0, 1.0);
        assert!(hi > lo, "F7 should be monotonic up: lo={lo} hi={hi}");
    }

    #[test]
    fn mood_zero_silences_the_source() {
        // mood ($f5) is a multiplicative pre-curve weight on every
        // formula (except F8 which has a `+ vol` survivor); F7 with
        // d=0 has no offset, so mood=0 must give exactly 0.
        let p = SlotParams {
            steepness: 0.4,
            deviation: 0.0,
            maximum: 1.0,
            minimum: 0.0,
            original_level: 1.0,
            formula: 7,
        };
        assert!(p.eval(0.5, 0.0).abs() < 1e-6);
        assert!(p.eval(0.5, 1.0) > 0.0);
    }

    #[test]
    fn clip_uses_minimum_floor() {
        // minimum > 0 should lift a zero curve up to the floor.
        let p = SlotParams {
            steepness: 0.4,
            deviation: 0.0,
            maximum: 1.0,
            minimum: 0.3,
            original_level: 1.0,
            formula: 7,
        };
        // mood=0 → curve(...)=0 → clipped up to minimum
        let v = p.eval(0.0, 0.0);
        assert!((v - 0.3).abs() < 1e-6, "expected 0.3 (floor), got {v}");
    }

    #[test]
    fn slew_reaches_target_within_smooth_window() {
        let mut s = Slew::default();
        // smooth_ms = 100; if we step at dt = 100 ms once, we should reach the target.
        let v = s.step(1.0, 100.0, 0.1);
        assert!((v - 1.0).abs() < 1e-6, "expected 1.0, got {v}");
    }

    #[test]
    fn slew_partial_progress_at_half_window() {
        let mut s = Slew::default();
        let v = s.step(1.0, 100.0, 0.05);
        assert!((v - 0.5).abs() < 1e-3, "expected ~0.5, got {v}");
    }

    #[test]
    fn step_no_op_when_disabled() {
        let m = AdaptiveMixer::new();
        let params = HubParams::new(8);
        let before = params.sources[0].load_gain();
        let mut m = m;
        m.step(&params);
        assert_eq!(params.sources[0].load_gain(), before);
    }

    #[test]
    fn step_writes_gains_when_enabled() {
        let mut m = AdaptiveMixer::new();
        let params = HubParams::new(8);
        m.set_enabled(true);
        m.intensity = 0.5;
        // smooth_ms small + multiple steps so the slew converges.
        m.smooth_ms = 10.0;
        for _ in 0..50 {
            m.step(&params);
        }
        let g = params.sources[0].load_gain();
        assert!(g.is_finite() && g >= 0.0);
    }
}

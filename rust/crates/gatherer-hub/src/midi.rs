//! MIDI Clock + transport sync from a DAW into the hub.
//!
//! On macOS/Linux the hub creates a **virtual MIDI input port** named
//! "Gatherer Hub" — it shows up as a MIDI destination in any DAW. The
//! DAW routes its MIDI clock there; this module parses:
//!   - Clock (0xF8, 24 ppqn) → BPM (smoothed over one beat).
//!   - Start (0xFA) / Continue (0xFB) / Stop (0xFC) → transport flag.
//!   - Song Position Pointer (0xF2 lsb msb) → bar/beat position.
//!
//! The parser runs on midir's callback thread and publishes everything
//! into a shared `GridState` (atomics) the UI can read.

use atomic_float::AtomicF32;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// 24 MIDI Clock pulses per quarter note (standard).
pub const PPQN: u32 = 24;
/// Smooth BPM over one beat of clock pulses.
const SMOOTH_PULSES: usize = PPQN as usize;
/// v1 assumption — time-signature controls come later.
pub const BEATS_PER_BAR: u32 = 4;

pub struct GridState {
    bpm: AtomicF32,
    playing: AtomicBool,
    /// Total clock pulses since transport start (24 ppqn).
    pulses: AtomicU64,
    /// True once any clock byte has been received (UI uses this to
    /// distinguish "no MIDI routed yet" from "stopped at bar 1").
    connected: AtomicBool,
}

impl GridState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            bpm: AtomicF32::new(0.0),
            playing: AtomicBool::new(false),
            pulses: AtomicU64::new(0),
            connected: AtomicBool::new(false),
        })
    }

    #[inline]
    pub fn bpm(&self) -> f32 {
        self.bpm.load(Ordering::Relaxed)
    }
    #[inline]
    pub fn playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }
    #[inline]
    pub fn pulses(&self) -> u64 {
        self.pulses.load(Ordering::Relaxed)
    }
    #[inline]
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// (bar, beat), both 1-based, under the v1 4/4 assumption.
    pub fn bar_beat(&self) -> (u32, u32) {
        bar_beat_for(self.pulses())
    }
}

fn bar_beat_for(pulses: u64) -> (u32, u32) {
    let total_beats = pulses / PPQN as u64;
    let bar = (total_beats / BEATS_PER_BAR as u64) as u32 + 1;
    let beat = (total_beats % BEATS_PER_BAR as u64) as u32 + 1;
    (bar, beat)
}

/// Holds the open virtual MIDI input. Dropping closes the port and
/// stops the parser thread.
pub struct MidiSync {
    #[cfg(unix)]
    _conn: midir::MidiInputConnection<ParserState>,
    pub state: Arc<GridState>,
}

struct ParserState {
    grid: Arc<GridState>,
    last_clock_us: Option<u64>,
    interval_s: [f32; SMOOTH_PULSES],
    idx: usize,
    count: usize,
}

/// Create the virtual MIDI input port (name shown to the DAW) and start
/// parsing. Returns Err if midir can't open the port (e.g. on Windows,
/// where virtual port creation is not supported).
#[cfg(unix)]
pub fn start(port_name: &str) -> Result<MidiSync, String> {
    use midir::os::unix::VirtualInput;

    let grid = GridState::new();
    let input = midir::MidiInput::new("Gatherer Hub MIDI")
        .map_err(|e| format!("midir init: {e}"))?;

    let parser = ParserState {
        grid: grid.clone(),
        last_clock_us: None,
        interval_s: [0.0; SMOOTH_PULSES],
        idx: 0,
        count: 0,
    };

    let conn = input
        .create_virtual(port_name, on_message, parser)
        .map_err(|e| format!("create virtual MIDI input: {e}"))?;

    Ok(MidiSync {
        _conn: conn,
        state: grid,
    })
}

#[cfg(not(unix))]
pub fn start(_port_name: &str) -> Result<MidiSync, String> {
    Err("virtual MIDI input not supported on this platform".to_string())
}

fn on_message(ts_us: u64, msg: &[u8], state: &mut ParserState) {
    if msg.is_empty() {
        return;
    }
    state.grid.connected.store(true, Ordering::Relaxed);
    match msg[0] {
        0xF8 => {
            // Clock pulse — derive BPM from inter-pulse interval.
            if let Some(last) = state.last_clock_us {
                let dt_s = (ts_us.wrapping_sub(last)) as f32 * 1e-6;
                if dt_s > 0.0 && dt_s < 1.0 {
                    state.interval_s[state.idx] = dt_s;
                    state.idx = (state.idx + 1) % SMOOTH_PULSES;
                    state.count = (state.count + 1).min(SMOOTH_PULSES);
                    let avg = state.interval_s[..state.count].iter().sum::<f32>()
                        / state.count as f32;
                    let bpm = 60.0 / (avg * PPQN as f32);
                    state.grid.bpm.store(bpm.clamp(20.0, 999.0), Ordering::Relaxed);
                }
            }
            state.last_clock_us = Some(ts_us);
            state.grid.pulses.fetch_add(1, Ordering::Relaxed);
        }
        0xFA => {
            // Start: rewind + play.
            state.grid.pulses.store(0, Ordering::Relaxed);
            state.grid.playing.store(true, Ordering::Relaxed);
            state.last_clock_us = None;
            state.count = 0;
        }
        0xFB => {
            // Continue: resume from current position.
            state.grid.playing.store(true, Ordering::Relaxed);
            state.last_clock_us = None; // reset BPM smoothing
            state.count = 0;
        }
        0xFC => {
            // Stop.
            state.grid.playing.store(false, Ordering::Relaxed);
        }
        0xF2 if msg.len() >= 3 => {
            // Song Position Pointer: 14-bit count of MIDI beats (16th notes).
            // 1 SPP unit = 6 clock pulses (= 1/4 of a quarter at 24 ppqn).
            let lsb = msg[1] as u16 & 0x7F;
            let msb = msg[2] as u16 & 0x7F;
            let sixteenths = ((msb << 7) | lsb) as u64;
            state.grid.pulses.store(sixteenths * 6, Ordering::Relaxed);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_beat_arithmetic_4_4() {
        assert_eq!(bar_beat_for(0), (1, 1));
        assert_eq!(bar_beat_for(23), (1, 1)); // mid-beat
        assert_eq!(bar_beat_for(24), (1, 2)); // 1 beat = 24 pulses
        assert_eq!(bar_beat_for(72), (1, 4)); // 3 beats
        assert_eq!(bar_beat_for(96), (2, 1)); // 4 beats = 1 bar
        assert_eq!(bar_beat_for(96 * 3), (4, 1));
    }

    #[test]
    fn spp_sets_pulses() {
        let grid = GridState::new();
        let mut parser = ParserState {
            grid: grid.clone(),
            last_clock_us: None,
            interval_s: [0.0; SMOOTH_PULSES],
            idx: 0,
            count: 0,
        };
        // SPP 16 = 16 sixteenths = 4 beats = 1 bar in 4/4 = 96 pulses.
        on_message(0, &[0xF2, 16, 0], &mut parser);
        assert_eq!(grid.pulses(), 96);
        assert_eq!(grid.bar_beat(), (2, 1));
    }

    #[test]
    fn start_resets_position_and_starts_transport() {
        let grid = GridState::new();
        let mut parser = ParserState {
            grid: grid.clone(),
            last_clock_us: None,
            interval_s: [0.0; SMOOTH_PULSES],
            idx: 0,
            count: 0,
        };
        on_message(0, &[0xF2, 16, 0], &mut parser); // pos = 96
        on_message(0, &[0xFA], &mut parser); // Start
        assert_eq!(grid.pulses(), 0);
        assert!(grid.playing());
        on_message(0, &[0xFC], &mut parser); // Stop
        assert!(!grid.playing());
    }
}

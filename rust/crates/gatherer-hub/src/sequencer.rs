//! Adaptive section sequencer — the playback state machine that walks
//! the intro → main → outro graph the way the engine will, so the
//! author can hear transition choices.
//!
//! This module is **pure logic**: given section lengths + transition
//! regions, [`Sequencer::advance`] reports which section(s) should be
//! sounding at the current playback frame and at what gain. The audio
//! layer maps that [`Mix`] onto real PCM. No audio I/O lives here, which
//! is what makes the transition math unit-testable.
//!
//! ## Transition model (mirrors the `Region` spec in
//! [`crate::navigator::model`])
//!
//! A transition A → B is anchored at a single **sync instant** where
//! both clips are at 100%:
//! - in A's local time, sync = `from_out.sync_frames` (≈ `begin`),
//! - in B's local time, sync = `to_in.sync_frames` (≈ `end`).
//!
//! B is triggered early so its in-region fade-in lands at 100% exactly
//! at sync; A then fades out across its out-region *after* sync. The two
//! fade windows are independent lengths. `fade_pct == 0` ⇒ the engine
//! applies no envelope (the audio is pre-faded), but the region length
//! still governs sync alignment.
//!
//! ## Graph rules
//! - **Intro** plays once and auto-advances into Main at its out-region.
//! - **Main** loops (`loop_range`) indefinitely until [`trigger_exit`],
//!   which picks the next out-region (first whose `begin > head`) and
//!   crosses into Outro.
//! - **Outro** plays once and ends (→ Idle).
//!
//! The engine lands ahead of its audio-output consumer (the cpal wiring
//! is a follow-up), so it allows dead code until that integration calls
//! the full surface.
#![allow(dead_code)]

use crate::navigator::model::{Region, SectionKind};

/// One section's playback geometry (no PCM — the audio layer holds that
/// and indexes it by `section` + `frame`).
#[derive(Debug, Clone)]
pub struct AdaptiveSection {
    pub kind: SectionKind,
    /// Total length in frames.
    pub length: u64,
    /// Loop window `(begin, end)` in frames; `None` ⇒ play once.
    pub loop_range: Option<(u64, u64)>,
    pub in_regions: Vec<Region>,
    pub out_regions: Vec<Region>,
}

impl AdaptiveSection {
    /// The single entry region (intro/main/outro each have one canonical
    /// in-region). Falls back to a zero-length region at frame 0.
    fn entry_region(&self) -> Region {
        self.in_regions
            .first()
            .copied()
            .unwrap_or(Region::new_in(0, 0, 0.0, 0.5))
    }

    /// The out-region used for an exit, given the current `head`: the
    /// first whose `begin > head`, else the last one (so a late trigger
    /// still exits), else a zero-length region at the section end.
    fn exit_region(&self, head: u64) -> Region {
        self.out_regions
            .iter()
            .find(|r| r.begin_frames > head)
            .or_else(|| self.out_regions.last())
            .copied()
            .unwrap_or_else(|| Region::new_out(self.length, self.length, 0.0, 0.5))
    }
}

/// A sounding voice: which section, its local playhead, and its gain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Voice {
    pub section: usize,
    pub frame: u64,
    pub gain: f32,
}

/// What should be sounding this block. `a` is the primary / outgoing
/// voice, `b` the incoming voice during a cross.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Mix {
    pub a: Option<Voice>,
    pub b: Option<Voice>,
}

#[derive(Debug, Clone)]
struct PendingExit {
    out: Region,
    to: usize,
    to_in: Region,
}

#[derive(Debug, Clone)]
enum Phase {
    Idle,
    Playing {
        sec: usize,
        head: u64,
        pending: Option<PendingExit>,
    },
    Crossing {
        from: usize,
        from_head: u64,
        from_out: Region,
        to: usize,
        to_head: u64,
        to_in: Region,
    },
}

pub struct Sequencer {
    sections: Vec<AdaptiveSection>,
    phase: Phase,
}

impl Sequencer {
    pub fn new(sections: Vec<AdaptiveSection>) -> Self {
        Self {
            sections,
            phase: Phase::Idle,
        }
    }

    fn index_of(&self, kind: SectionKind) -> Option<usize> {
        self.sections.iter().position(|s| s.kind == kind)
    }

    /// Begin playback at the Intro (auto-arming the cross into Main) or,
    /// if there's no Intro, at the start of Main's loop.
    pub fn start(&mut self) {
        if let Some(intro) = self.index_of(SectionKind::Intro) {
            let pending = self.arm_exit_from(intro, 0, SectionKind::Main);
            self.phase = Phase::Playing {
                sec: intro,
                head: 0,
                pending,
            };
        } else if let Some(main) = self.index_of(SectionKind::Main) {
            let head = self.sections[main].loop_range.map(|(b, _)| b).unwrap_or(0);
            self.phase = Phase::Playing {
                sec: main,
                head,
                pending: None,
            };
        } else {
            self.phase = Phase::Idle;
        }
    }

    pub fn stop(&mut self) {
        self.phase = Phase::Idle;
    }

    pub fn is_playing(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    pub fn current_section(&self) -> Option<usize> {
        match &self.phase {
            Phase::Idle => None,
            Phase::Playing { sec, .. } => Some(*sec),
            Phase::Crossing { from, .. } => Some(*from),
        }
    }

    /// True once an exit has been armed (a transition is queued).
    pub fn is_exit_pending(&self) -> bool {
        matches!(
            &self.phase,
            Phase::Playing {
                pending: Some(_),
                ..
            } | Phase::Crossing { .. }
        )
    }

    /// Request a deferred exit. Only meaningful while playing Main: arms
    /// the next out-region → Outro (or → Idle if there's no Outro).
    pub fn trigger_exit(&mut self) {
        // Read what we need first, then compute the pending exit, then
        // commit — avoids overlapping borrows of `self`.
        let (sec, head) = match &self.phase {
            Phase::Playing {
                sec,
                head,
                pending: None,
            } if self.sections.get(*sec).map(|s| s.kind) == Some(SectionKind::Main) => {
                (*sec, *head)
            }
            _ => return,
        };
        let exit = self.arm_exit_from(sec, head, SectionKind::Outro);
        if let Phase::Playing { pending, .. } = &mut self.phase {
            *pending = exit;
        }
    }

    /// Build a PendingExit from `sec` (current `head`) into the section
    /// of `to_kind`. Returns `None` if the target section is absent.
    fn arm_exit_from(&self, sec: usize, head: u64, to_kind: SectionKind) -> Option<PendingExit> {
        let to = self.index_of(to_kind)?;
        let out = self.sections[sec].exit_region(head);
        let to_in = self.sections[to].entry_region();
        Some(PendingExit { out, to, to_in })
    }

    /// Advance the clock by `n` frames and report what should sound.
    pub fn advance(&mut self, n: u64) -> Mix {
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Idle => Mix::default(),
            Phase::Playing {
                sec,
                head,
                pending,
            } => self.advance_playing(sec, head, pending, n),
            Phase::Crossing {
                from,
                from_head,
                from_out,
                to,
                to_head,
                to_in,
            } => self.advance_crossing(from, from_head, from_out, to, to_head, to_in, n),
        }
    }

    fn advance_playing(
        &mut self,
        sec: usize,
        head: u64,
        pending: Option<PendingExit>,
        n: u64,
    ) -> Mix {
        let section = &self.sections[sec];
        let new_head = advance_head(head, n, section.loop_range);

        if let Some(pe) = pending {
            // B must start `in_len` frames before A reaches the sync.
            let in_len = region_len(&pe.to_in);
            let b_start_head = pe.out.begin_frames.saturating_sub(in_len);
            if new_head >= b_start_head {
                // Enter the cross. B starts at its in-region begin; if we
                // overshot the start point within this block, advance B
                // by the overshoot so the heads stay sync-aligned.
                let overshoot = new_head - b_start_head;
                let to_head = pe.to_in.begin_frames + overshoot;
                let from_out = pe.out;
                let to = pe.to;
                let to_in = pe.to_in;
                let mix = cross_mix(sec, new_head, &from_out, to, to_head, &to_in);
                // If A is already past its out-region end, the cross is
                // instantaneous → land in Playing(to).
                if new_head >= from_out.end_frames {
                    self.enter_playing(to, to_head);
                } else {
                    self.phase = Phase::Crossing {
                        from: sec,
                        from_head: new_head,
                        from_out,
                        to,
                        to_head,
                        to_in,
                    };
                }
                return mix;
            }
            // Still before B's start — keep playing A solo, hold pending.
            self.phase = Phase::Playing {
                sec,
                head: new_head,
                pending: Some(pe),
            };
            return Mix {
                a: Some(Voice {
                    section: sec,
                    frame: new_head,
                    gain: 1.0,
                }),
                b: None,
            };
        }

        // No pending exit. A non-looping section that ran off the end
        // ends playback (Outro → Idle).
        if section.loop_range.is_none() && new_head >= section.length {
            self.phase = Phase::Idle;
            return Mix::default();
        }
        self.phase = Phase::Playing {
            sec,
            head: new_head,
            pending: None,
        };
        Mix {
            a: Some(Voice {
                section: sec,
                frame: new_head,
                gain: 1.0,
            }),
            b: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn advance_crossing(
        &mut self,
        from: usize,
        from_head: u64,
        from_out: Region,
        to: usize,
        to_head: u64,
        to_in: Region,
        n: u64,
    ) -> Mix {
        // A keeps playing forward (no loop wrap during an exit cross);
        // B advances toward and past its sync.
        let new_from = from_head + n;
        let new_to = to_head + n;
        let mix = cross_mix(from, new_from, &from_out, to, new_to, &to_in);
        if new_from >= from_out.end_frames {
            // A's fade-out is done — B becomes the sole section.
            self.enter_playing(to, new_to);
        } else {
            self.phase = Phase::Crossing {
                from,
                from_head: new_from,
                from_out,
                to,
                to_head: new_to,
                to_in,
            };
        }
        mix
    }

    /// Land in `Playing(to)`. If `to` is the Main section it loops with
    /// no pending exit; Outro plays once; Intro (rare here) would
    /// auto-advance again.
    fn enter_playing(&mut self, to: usize, head: u64) {
        let pending = match self.sections.get(to).map(|s| s.kind) {
            // Intro reached via a cross would re-arm toward Main, but the
            // normal graph never crosses *into* Intro. Main + Outro carry
            // no auto-pending (Main waits for trigger_exit; Outro ends).
            _ => None,
        };
        self.phase = Phase::Playing {
            sec: to,
            head,
            pending,
        };
    }
}

/// Advance a head by `n`, wrapping through a loop window if present.
fn advance_head(head: u64, n: u64, loop_range: Option<(u64, u64)>) -> u64 {
    let next = head + n;
    if let Some((begin, end)) = loop_range {
        if end > begin && next >= end {
            let span = end - begin;
            return begin + (next - end) % span;
        }
    }
    next
}

fn region_len(r: &Region) -> u64 {
    r.end_frames.saturating_sub(r.begin_frames)
}

/// Gain of an in-region (fade-in) at local frame `local`. Sync at
/// `sync_frames`; the fade occupies the last `len*fade_pct` frames
/// ending at sync. `fade_pct == 0` ⇒ full gain throughout (pre-faded).
fn in_gain(r: &Region, local: u64) -> f32 {
    if r.fade_pct <= 0.0 {
        return 1.0;
    }
    let len = region_len(r) as f32;
    let fade_len = (len * r.fade_pct).max(1.0);
    let fade_start = r.sync_frames.saturating_sub(fade_len as u64);
    if local <= fade_start {
        0.0
    } else if local >= r.sync_frames {
        1.0
    } else {
        let x = (local - fade_start) as f32 / fade_len;
        shape(x, r.fade_shape)
    }
}

/// Gain of an out-region (fade-out) at local frame `local`. Sync at
/// `sync_frames`; the fade occupies the first `len*fade_pct` frames
/// starting at sync. `fade_pct == 0` ⇒ full gain until the region end.
fn out_gain(r: &Region, local: u64) -> f32 {
    if r.fade_pct <= 0.0 {
        return 1.0;
    }
    let len = region_len(r) as f32;
    let fade_len = (len * r.fade_pct).max(1.0);
    let fade_end = r.sync_frames + fade_len as u64;
    if local <= r.sync_frames {
        1.0
    } else if local >= fade_end {
        0.0
    } else {
        let x = (local - r.sync_frames) as f32 / fade_len;
        1.0 - shape(x, r.fade_shape)
    }
}

/// Map a normalised 0..1 fade position through the shape parameter.
/// `0.5` ≈ linear; higher pushes toward an equal-power-ish curve. Kept
/// simple for v1 (linear); `fade_shape` reserved for a real curve.
fn shape(x: f32, _fade_shape: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

/// Build the two-voice mix during a cross.
fn cross_mix(
    from: usize,
    from_head: u64,
    from_out: &Region,
    to: usize,
    to_head: u64,
    to_in: &Region,
) -> Mix {
    let a_gain = out_gain(from_out, from_head);
    let b_gain = in_gain(to_in, to_head);
    let a = (a_gain > 0.0 || from_head <= from_out.sync_frames).then_some(Voice {
        section: from,
        frame: from_head,
        gain: a_gain,
    });
    let b = Some(Voice {
        section: to,
        frame: to_head,
        gain: b_gain,
    });
    Mix { a, b }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intro() -> AdaptiveSection {
        AdaptiveSection {
            kind: SectionKind::Intro,
            length: 1000,
            loop_range: None,
            in_regions: vec![Region::new_in(0, 0, 0.0, 0.5)],
            // exit sync at 800, fade-out over [800, 900].
            out_regions: vec![Region::new_out(800, 900, 1.0, 0.5)],
        }
    }

    fn main_section() -> AdaptiveSection {
        AdaptiveSection {
            kind: SectionKind::Main,
            length: 4000,
            loop_range: Some((0, 4000)),
            // entry fade-in over [0, 100], sync at 100.
            in_regions: vec![Region::new_in(0, 100, 1.0, 0.5)],
            // three exit windows.
            out_regions: vec![
                Region::new_out(1000, 1100, 1.0, 0.5),
                Region::new_out(2000, 2100, 1.0, 0.5),
                Region::new_out(3000, 3100, 1.0, 0.5),
            ],
        }
    }

    fn outro() -> AdaptiveSection {
        AdaptiveSection {
            kind: SectionKind::Outro,
            length: 1000,
            loop_range: None,
            in_regions: vec![Region::new_in(0, 100, 1.0, 0.5)],
            out_regions: vec![Region::new_out(900, 1000, 0.0, 0.5)],
        }
    }

    #[test]
    fn intro_to_main_sync_alignment() {
        // Intro (out sync 800) → Main (in sync 100). At the sync instant
        // both voices should be at full gain, A at frame 800, B at 100.
        let mut seq = Sequencer::new(vec![intro(), main_section()]);
        seq.start();
        // Step 1 frame at a time until A reaches its sync (frame 800).
        let mut last = Mix::default();
        for _ in 0..2000 {
            last = seq.advance(1);
            if let Some(a) = last.a {
                if a.section == 0 && a.frame == 800 {
                    break;
                }
            }
        }
        let a = last.a.expect("outgoing voice present at sync");
        let b = last.b.expect("incoming voice present at sync");
        assert_eq!(a.section, 0, "A is the intro");
        assert_eq!(a.frame, 800, "A at its out sync");
        assert!((a.gain - 1.0).abs() < 1e-3, "A at full gain at sync");
        assert_eq!(b.section, 1, "B is main");
        assert_eq!(b.frame, 100, "B at its in sync");
        assert!((b.gain - 1.0).abs() < 1e-3, "B at full gain at sync");
    }

    #[test]
    fn main_loops_indefinitely() {
        // No intro → start on Main. Without an exit trigger it never
        // transitions and the head stays within the loop window.
        let mut seq = Sequencer::new(vec![main_section()]);
        seq.start();
        let loop_end = 4000u64;
        for _ in 0..(loop_end / 100 * 3 + 5) {
            let mix = seq.advance(100);
            let a = mix.a.expect("main always sounding");
            assert_eq!(a.section, 0);
            assert!(a.frame < loop_end, "head stays inside the loop");
            assert!(mix.b.is_none(), "no cross without a trigger");
        }
        assert!(!seq.is_exit_pending());
        assert_eq!(seq.current_section(), Some(0));
    }

    #[test]
    fn deferred_exit_picks_next_outregion() {
        // Trigger exit at head≈1500 → next out-region is the one at 2000
        // (first whose begin > head), not the passed 1000 one.
        let mut seq = Sequencer::new(vec![main_section(), outro()]);
        seq.start();
        // Advance to ~1500.
        seq.advance(1500);
        seq.trigger_exit();
        assert!(seq.is_exit_pending(), "exit armed");
        // B (outro) starts in_len(=100) before the 2000 sync, i.e. at
        // head 1900. Step until the cross begins, then the sync lands at
        // A frame 2000.
        let mut synced = None;
        for _ in 0..2000 {
            let mix = seq.advance(1);
            if let (Some(a), Some(_b)) = (mix.a, mix.b) {
                if a.section == 0 && a.frame == 2000 {
                    synced = Some(mix);
                    break;
                }
            }
        }
        let mix = synced.expect("reached the 2000 sync");
        assert_eq!(mix.a.unwrap().frame, 2000, "exit used the 2000 window");
        assert_eq!(mix.b.unwrap().section, 1, "crossing into outro");
        assert!((mix.b.unwrap().gain - 1.0).abs() < 1e-3);
    }

    #[test]
    fn fade_pct_zero_skips_envelope() {
        // An out-region with fade_pct=0 stays at full gain across its
        // whole span (no engine ramp); an in-region with fade_pct=0 is
        // full from the start.
        let out0 = Region::new_out(1000, 1100, 0.0, 0.5);
        assert_eq!(out_gain(&out0, 1000), 1.0);
        assert_eq!(out_gain(&out0, 1050), 1.0, "no ramp mid-region");
        assert_eq!(out_gain(&out0, 1099), 1.0);

        let in0 = Region::new_in(0, 100, 0.0, 0.5);
        assert_eq!(in_gain(&in0, 0), 1.0, "full from the start");
        assert_eq!(in_gain(&in0, 50), 1.0);
        assert_eq!(in_gain(&in0, 100), 1.0);

        // Contrast: fade_pct=1 ramps.
        let in1 = Region::new_in(0, 100, 1.0, 0.5);
        assert!(in_gain(&in1, 0) < 0.1, "ramps from ~0");
        assert!((in_gain(&in1, 100) - 1.0).abs() < 1e-3, "reaches 1 at sync");
        assert!((in_gain(&in1, 50) - 0.5).abs() < 0.05, "linear midpoint");
    }

    #[test]
    fn full_sequence_intro_main_outro_to_idle() {
        let mut seq = Sequencer::new(vec![intro(), main_section(), outro()]);
        seq.start();
        // Play through the intro→main cross, loop main a bit, exit.
        for _ in 0..50 {
            seq.advance(100);
        }
        assert_eq!(
            seq.current_section(),
            Some(1),
            "settled into main after intro cross"
        );
        seq.trigger_exit();
        // Run long enough to cross to outro and let it finish.
        let mut ended = false;
        for _ in 0..200 {
            let mix = seq.advance(100);
            if mix.a.is_none() && mix.b.is_none() {
                ended = true;
                break;
            }
        }
        assert!(ended, "outro played out and the sequencer went idle");
        assert!(!seq.is_playing());
    }
}

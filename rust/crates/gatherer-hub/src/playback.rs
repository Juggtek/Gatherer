//! Recorded-take playback + waveform envelopes.
//!
//! The UI loads the just-recorded WAVs into memory (off the audio thread)
//! and publishes them to the output callback via an `ArcSwapOption`. While
//! `playing`, the output callback sums the sources at a shared playhead.
//! Transport (play/pause/stop) and position are plain atomics.

use arc_swap::ArcSwapOption;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Waveform display resolution (peak buckets per source).
const ENVELOPE_BUCKETS: usize = 1200;

/// Decoded take: interleaved-stereo samples per recorded source, plus
/// the MIDI grid state captured at the moment recording started (so the
/// UI can draw bar lines over the waveforms). `time_sig_num` is the
/// beats-per-bar in effect at the time of recording.
pub struct PlaybackData {
    pub sources: Vec<Arc<Vec<f32>>>,
    pub len_frames: usize,
    pub start_pulses: u64,
    pub bpm: f32,
    /// Meter at record-start (kept for future session persistence; the
    /// timeline display uses the live UI meter so it can be corrected).
    #[allow(dead_code)]
    pub time_sig_num: u32,
}

/// Shared transport + buffers between UI (control + load) and audio (read).
pub struct Playback {
    playing: AtomicBool,
    position: AtomicU64, // frame index
    len_frames: AtomicU64,
    data: ArcSwapOption<PlaybackData>,
}

impl Playback {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            playing: AtomicBool::new(false),
            position: AtomicU64::new(0),
            len_frames: AtomicU64::new(0),
            data: ArcSwapOption::empty(),
        })
    }

    /// Publish a freshly-loaded take (resets transport to the start).
    pub fn set_take(&self, data: PlaybackData) {
        self.playing.store(false, Ordering::Relaxed);
        self.position.store(0, Ordering::Relaxed);
        self.len_frames.store(data.len_frames as u64, Ordering::Relaxed);
        self.data.store(Some(Arc::new(data)));
    }

    pub fn has_take(&self) -> bool {
        self.data.load().is_some()
    }
    /// Audio-thread snapshot of the current buffers (cheap Arc clone).
    pub fn snapshot(&self) -> Option<Arc<PlaybackData>> {
        self.data.load_full()
    }

    pub fn play(&self) {
        if self.has_take() {
            if self.position() >= self.len_frames() {
                self.position.store(0, Ordering::Relaxed); // restart from end
            }
            self.playing.store(true, Ordering::Relaxed);
        }
    }
    pub fn pause(&self) {
        self.playing.store(false, Ordering::Relaxed);
    }
    pub fn stop(&self) {
        self.playing.store(false, Ordering::Relaxed);
        self.position.store(0, Ordering::Relaxed);
    }

    #[inline]
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }
    #[inline]
    pub fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }
    #[inline]
    pub fn set_position(&self, p: u64) {
        self.position.store(p, Ordering::Relaxed);
    }
    pub fn len_frames(&self) -> u64 {
        self.len_frames.load(Ordering::Relaxed)
    }
    /// Playhead position as a 0..1 fraction of the take length.
    #[allow(dead_code)] // superseded by the timeline's pixel-based playhead
    pub fn fraction(&self) -> f32 {
        let len = self.len_frames();
        if len == 0 {
            0.0
        } else {
            (self.position() as f32 / len as f32).clamp(0.0, 1.0)
        }
    }
}

/// Read the recorded WAVs for `sources` (1-based file names) from `dir`,
/// returning the in-memory take plus a peak envelope per source for the
/// waveform display. Runs on the UI thread (file I/O, allocation).
pub fn load_take(
    dir: &Path,
    sources: &[usize],
    start_pulses: u64,
    bpm: f32,
    time_sig_num: u32,
) -> Result<(PlaybackData, Vec<(usize, Vec<f32>)>), String> {
    let mut bufs = Vec::with_capacity(sources.len());
    let mut envs = Vec::with_capacity(sources.len());
    let mut max_len = 0usize;

    for &s in sources {
        let path = dir.join(format!("source-{:02}.wav", s + 1));
        let mut reader = hound::WavReader::open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        let samples: Vec<f32> = reader.samples::<f32>().filter_map(|r| r.ok()).collect();
        max_len = max_len.max(samples.len() / 2);
        envs.push((s, envelope(&samples)));
        bufs.push(Arc::new(samples));
    }

    Ok((
        PlaybackData {
            sources: bufs,
            len_frames: max_len,
            start_pulses,
            bpm,
            time_sig_num: time_sig_num.max(1),
        },
        envs,
    ))
}

/// Per-bucket peak (max |L|,|R|) over interleaved-stereo samples.
fn envelope(interleaved: &[f32]) -> Vec<f32> {
    let frames = interleaved.len() / 2;
    if frames == 0 {
        return vec![0.0];
    }
    let buckets = ENVELOPE_BUCKETS.min(frames);
    let per = frames as f32 / buckets as f32;
    let mut env = vec![0.0f32; buckets];
    for (b, slot) in env.iter_mut().enumerate() {
        let start = (b as f32 * per) as usize;
        let end = (((b + 1) as f32 * per) as usize).min(frames);
        let mut peak = 0.0f32;
        for f in start..end {
            peak = peak.max(interleaved[f * 2].abs()).max(interleaved[f * 2 + 1].abs());
        }
        *slot = peak;
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_captures_peak_per_bucket() {
        // 2400 frames; a spike of 0.9 near the middle should land in one bucket.
        let mut s = vec![0.0f32; 2400 * 2];
        s[1200 * 2] = 0.9;
        s[1200 * 2 + 1] = -0.9;
        let env = envelope(&s);
        assert_eq!(env.len(), ENVELOPE_BUCKETS.min(2400));
        let max = env.iter().cloned().fold(0.0f32, f32::max);
        assert!((max - 0.9).abs() < 1e-6, "peak bucket should be 0.9, got {max}");
    }

    #[test]
    fn transport_play_pause_stop() {
        let pb = Playback::new();
        assert!(!pb.has_take());
        pb.play(); // no-op without a take
        assert!(!pb.is_playing());

        pb.set_take(PlaybackData {
            sources: vec![Arc::new(vec![0.0; 100 * 2])],
            len_frames: 100,
            start_pulses: 0,
            bpm: 0.0,
            time_sig_num: 4,
        });
        assert!(pb.has_take());
        pb.play();
        assert!(pb.is_playing());
        pb.set_position(50);
        pb.pause();
        assert!(!pb.is_playing());
        assert_eq!(pb.position(), 50); // pause keeps position
        pb.stop();
        assert_eq!(pb.position(), 0); // stop rewinds

        // play from the end restarts at 0
        pb.set_position(100);
        pb.play();
        assert_eq!(pb.position(), 0);
    }
}

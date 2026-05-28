//! Per-source recording to WAV. RT-safe by construction: the audio thread
//! pushes raw captured stereo into per-source `rtrb` rings; a background
//! writer thread drains them into `hound` WAV files. No file I/O ever
//! happens on the audio thread. Mirrors FIELD's LayerWriter split.
//!
//! Recording captures the **raw** per-source input (pre gain/mute/solo/
//! invert) — those are monitoring controls, not part of the captured take.

use atomic_float::AtomicF32;
use rtrb::{Consumer, Producer};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// ~1.36 s of stereo slack per source @ 48 k — absorbs writer scheduling.
const REC_RING_FRAMES: usize = 1 << 16;

/// Live-preview envelope: mono samples per bucket (= ½ that many stereo
/// frames). 4800 samples = 50 ms @ 48 k.
const PREVIEW_SAMPLES_PER_BUCKET: u32 = 4800;
/// 8192 buckets × 50 ms = ~6.8 min cap for the live preview.
const PREVIEW_MAX_BUCKETS: usize = 8192;

type Wav = hound::WavWriter<std::io::BufWriter<std::fs::File>>;

/// Shared record state. UI writes arm flags + active; audio thread reads.
#[derive(Debug)]
pub struct RecordState {
    active: AtomicBool,
    armed: Vec<AtomicBool>,
}

impl RecordState {
    pub fn new(num_sources: usize) -> Arc<Self> {
        Arc::new(Self {
            active: AtomicBool::new(false),
            armed: (0..num_sources).map(|_| AtomicBool::new(false)).collect(),
        })
    }
    pub fn num_sources(&self) -> usize {
        self.armed.len()
    }
    #[inline]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
    fn set_active(&self, on: bool) {
        self.active.store(on, Ordering::Relaxed);
    }
    #[inline]
    pub fn is_armed(&self, s: usize) -> bool {
        self.armed.get(s).map(|a| a.load(Ordering::Relaxed)).unwrap_or(false)
    }
    pub fn set_armed(&self, s: usize, on: bool) {
        if let Some(a) = self.armed.get(s) {
            a.store(on, Ordering::Relaxed);
        }
    }
    pub fn armed_count(&self) -> usize {
        self.armed.iter().filter(|a| a.load(Ordering::Relaxed)).count()
    }
}

pub enum WriterCommand {
    Start {
        dir: PathBuf,
        armed: Vec<usize>,
        sample_rate: u32,
        /// Shared live envelope the writer fills as it drains. UI reads it
        /// each tick to draw the growing waveform during recording.
        preview: Arc<RecordingPreview>,
    },
    Stop,
}

/// One source's live envelope. Writer thread (single writer per source)
/// stores peak per bucket; UI loads atomics each tick.
pub struct PreviewSource {
    pub source_idx: usize,
    pub envelope: Vec<AtomicF32>,
    pub current_bucket: AtomicUsize,
}

/// Shared live preview of an in-progress recording.
pub struct RecordingPreview {
    pub sources: Vec<PreviewSource>,
    pub sample_rate: u32,
    pub samples_per_bucket: u32,
}

impl RecordingPreview {
    pub fn new(armed: &[usize], sample_rate: u32) -> Arc<Self> {
        let sources = armed
            .iter()
            .map(|&idx| PreviewSource {
                source_idx: idx,
                envelope: (0..PREVIEW_MAX_BUCKETS)
                    .map(|_| AtomicF32::new(0.0))
                    .collect(),
                current_bucket: AtomicUsize::new(0),
            })
            .collect();
        Arc::new(Self {
            sources,
            sample_rate,
            samples_per_bucket: PREVIEW_SAMPLES_PER_BUCKET,
        })
    }

    /// Loaded-as-Vec snapshot of one source's filled buckets (UI helper).
    pub fn snapshot(&self, source_idx: usize) -> Vec<f32> {
        let Some(src) = self.sources.iter().find(|s| s.source_idx == source_idx) else {
            return Vec::new();
        };
        let n = src.current_bucket.load(Ordering::Relaxed).min(src.envelope.len());
        src.envelope[..n].iter().map(|a| a.load(Ordering::Relaxed)).collect()
    }

    /// Seconds elapsed for `source_idx` (bucket count × bucket duration).
    #[allow(dead_code)] // public helper; the UI currently inlines the math
    pub fn elapsed_seconds(&self, source_idx: usize) -> f32 {
        let Some(src) = self.sources.iter().find(|s| s.source_idx == source_idx) else {
            return 0.0;
        };
        let n = src.current_bucket.load(Ordering::Relaxed);
        // mono samples / 2 = stereo frames; / sample_rate = seconds
        n as f32 * (self.samples_per_bucket as f32 / 2.0) / self.sample_rate as f32
    }
}

/// UI-side handle: arm flags + start/stop. One per engine instance (the
/// rings + writer thread are owned by the engine; dropping this on engine
/// teardown disconnects the channel and the writer thread exits).
pub struct RecorderControl {
    tx: Sender<WriterCommand>,
    pub state: Arc<RecordState>,
    pub recording: bool,
    /// Root of the current/last session folder
    /// (`~/Music/Gatherer/<session_name>/`). Recording WAVs land in
    /// `<root>/recording/`; export writes `<root>/unnormalized/` and
    /// `<root>/normalized/`.
    pub last_session: Option<PathBuf>,
    /// Resolved session name (whatever the user typed, or auto-generated).
    pub last_session_name: Option<String>,
    /// Source indices captured in the last/current session (for loading the
    /// take back for playback + waveform display).
    pub last_armed: Vec<usize>,
    /// Live envelope of the current/last session — UI reads atomics each tick.
    pub last_preview: Option<Arc<RecordingPreview>>,
    pub error: Option<String>,
}

impl RecorderControl {
    pub fn new(tx: Sender<WriterCommand>, state: Arc<RecordState>) -> Self {
        Self {
            tx,
            state,
            recording: false,
            last_session: None,
            last_session_name: None,
            last_armed: Vec::new(),
            last_preview: None,
            error: None,
        }
    }

    /// `section_slug` namespaces the recording under
    /// `<root>/recording/<slug>/` so each adaptive section (intro / main
    /// / outro) records into its own folder. Pass `""` for the legacy
    /// flat `<root>/recording/` layout.
    pub fn toggle(&mut self, sample_rate: u32, session_name: &str, section_slug: &str) {
        if self.recording {
            self.stop();
        } else {
            self.start(sample_rate, session_name, section_slug);
        }
    }

    fn start(&mut self, sample_rate: u32, session_name: &str, section_slug: &str) {
        let armed: Vec<usize> = (0..self.state.num_sources())
            .filter(|&s| self.state.is_armed(s))
            .collect();
        if armed.is_empty() {
            self.error = Some("Arm at least one source (R) before recording".into());
            return;
        }
        // Resolve the session name (auto-fill with a timestamp if blank).
        let resolved_name = if session_name.trim().is_empty() {
            format!("session-{}", unix_secs())
        } else {
            session_name.trim().to_string()
        };
        let session_root = match session_root_for(&resolved_name) {
            Ok(p) => p,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        let recording_dir = if section_slug.is_empty() {
            session_root.join("recording")
        } else {
            session_root.join("recording").join(section_slug)
        };
        if let Err(e) = std::fs::create_dir_all(&recording_dir) {
            self.error = Some(format!("create {}: {e}", recording_dir.display()));
            return;
        }
        self.last_armed = armed.clone();
        let preview = RecordingPreview::new(&armed, sample_rate);
        self.last_preview = Some(preview.clone());
        self.state.set_active(true);
        if self
            .tx
            .send(WriterCommand::Start {
                dir: recording_dir,
                armed,
                sample_rate,
                preview,
            })
            .is_err()
        {
            self.state.set_active(false);
            self.error = Some("recorder thread is gone".into());
            return;
        }
        self.recording = true;
        self.last_session = Some(session_root);
        self.last_session_name = Some(resolved_name);
        self.error = None;
    }

    fn stop(&mut self) {
        self.state.set_active(false);
        let _ = self.tx.send(WriterCommand::Stop);
        self.recording = false;
    }
}

/// Create the per-source recording rings. Producers go to the audio
/// callback; consumers to the writer thread.
pub fn make_rings(num_sources: usize) -> (Vec<Producer<f32>>, Vec<Consumer<f32>>) {
    let mut prods = Vec::with_capacity(num_sources);
    let mut cons = Vec::with_capacity(num_sources);
    for _ in 0..num_sources {
        let (p, c) = rtrb::RingBuffer::<f32>::new(REC_RING_FRAMES * 2);
        prods.push(p);
        cons.push(c);
    }
    (prods, cons)
}

/// Background writer thread. Exits when the command channel disconnects
/// (its `RecorderControl` dropped on engine teardown) — draining and
/// finalizing any open files first.
pub fn writer_loop(mut consumers: Vec<Consumer<f32>>, cmd_rx: Receiver<WriterCommand>) {
    let n = consumers.len();
    let mut writers: Vec<Option<Wav>> = (0..n).map(|_| None).collect();
    let mut acc_peak: Vec<f32> = vec![0.0; n];
    let mut acc_samples: Vec<u32> = vec![0; n];
    let mut preview: Option<Arc<RecordingPreview>> = None;
    let mut recording = false;

    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(5)) {
            Ok(WriterCommand::Start { dir, armed, sample_rate, preview: p }) => {
                let spec = hound::WavSpec {
                    channels: 2,
                    sample_rate,
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                };
                for s in armed {
                    if s >= writers.len() {
                        continue;
                    }
                    let path = dir.join(format!("source-{:02}.wav", s + 1));
                    match hound::WavWriter::create(&path, spec) {
                        Ok(w) => writers[s] = Some(w),
                        Err(e) => eprintln!("recorder: create {}: {e}", path.display()),
                    }
                }
                for v in acc_peak.iter_mut() {
                    *v = 0.0;
                }
                for v in acc_samples.iter_mut() {
                    *v = 0;
                }
                preview = Some(p);
                recording = true;
            }
            Ok(WriterCommand::Stop) => {
                drain(&mut consumers, &mut writers, preview.as_deref(), &mut acc_peak, &mut acc_samples);
                // Catch a trailing block the audio thread may have pushed
                // after it observed active=false.
                std::thread::sleep(Duration::from_millis(20));
                drain(&mut consumers, &mut writers, preview.as_deref(), &mut acc_peak, &mut acc_samples);
                finalize(&mut writers);
                preview = None;
                recording = false;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                drain(&mut consumers, &mut writers, preview.as_deref(), &mut acc_peak, &mut acc_samples);
                finalize(&mut writers);
                break;
            }
        }
        if recording {
            drain(&mut consumers, &mut writers, preview.as_deref(), &mut acc_peak, &mut acc_samples);
        }
    }
}

fn drain(
    consumers: &mut [Consumer<f32>],
    writers: &mut [Option<Wav>],
    preview: Option<&RecordingPreview>,
    acc_peak: &mut [f32],
    acc_samples: &mut [u32],
) {
    let spb = preview.map(|p| p.samples_per_bucket).unwrap_or(u32::MAX);
    for (s, (c, w)) in consumers.iter_mut().zip(writers.iter_mut()).enumerate() {
        if let Some(writer) = w {
            let ps = preview.and_then(|p| p.sources.iter().find(|ps| ps.source_idx == s));
            let cap = ps.map(|p| p.envelope.len()).unwrap_or(0);
            while let Ok(sample) = c.pop() {
                if writer.write_sample(sample).is_err() {
                    break;
                }
                if let Some(ps) = ps {
                    let bucket = ps.current_bucket.load(Ordering::Relaxed);
                    if bucket < cap {
                        acc_peak[s] = acc_peak[s].max(sample.abs());
                        acc_samples[s] += 1;
                        if acc_samples[s] >= spb {
                            // Single writer per source for this bucket; non-decreasing.
                            ps.envelope[bucket].store(acc_peak[s], Ordering::Relaxed);
                            ps.current_bucket.store(bucket + 1, Ordering::Relaxed);
                            acc_peak[s] = 0.0;
                            acc_samples[s] = 0;
                        }
                    }
                }
            }
        }
    }
}

fn finalize(writers: &mut [Option<Wav>]) {
    for w in writers.iter_mut() {
        if let Some(writer) = w.take() {
            if let Err(e) = writer.finalize() {
                eprintln!("recorder: finalize: {e}");
            }
        }
    }
}

/// Recording folder for a section: `<root>/recording/<slug>/`, or the
/// flat `<root>/recording/` when `slug` is empty. Falls back to the
/// flat layout if the per-section folder doesn't exist but the flat one
/// does (so legacy single-take recordings still load).
pub fn recording_dir(session_root: &Path, slug: &str) -> PathBuf {
    if slug.is_empty() {
        return session_root.join("recording");
    }
    let per_section = session_root.join("recording").join(slug);
    let flat = session_root.join("recording");
    if !per_section.exists() && flat.join("source-01.wav").exists() {
        flat
    } else {
        per_section
    }
}

/// `~/Music/Gatherer/<name>/`, created. Single root for the whole session
/// (recording, unnormalized export, normalized export sit underneath).
fn session_root_for(name: &str) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME not set".to_string())?;
    let dir = home.join("Music").join("Gatherer").join(name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(dir)
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// End-to-end writer path with no audio hardware: push known stereo
    /// samples through a ring → writer thread → WAV file → read back and
    /// verify the bytes round-trip exactly.
    #[test]
    fn writer_records_pushed_samples_to_wav() {
        let (mut prods, cons) = make_rings(1);
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || writer_loop(cons, rx));

        let dir = std::env::temp_dir().join(format!("gatherer-rec-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        tx.send(WriterCommand::Start {
            dir: dir.clone(),
            armed: vec![0],
            sample_rate: 48_000,
            preview: RecordingPreview::new(&[0], 48_000),
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(30)); // let the file open

        // 4 stereo frames, interleaved L,R.
        let samples = [0.1f32, -0.1, 0.2, -0.2, 0.3, -0.3, 0.4, -0.4];
        for &s in &samples {
            while prods[0].push(s).is_err() {}
        }
        std::thread::sleep(Duration::from_millis(60)); // let the writer drain

        tx.send(WriterCommand::Stop).unwrap();
        std::thread::sleep(Duration::from_millis(80)); // Stop drains + finalizes
        drop(tx); // disconnect → writer thread exits
        handle.join().unwrap();

        let path = dir.join("source-01.wav");
        let mut reader = hound::WavReader::open(&path).expect("wav should exist");
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().sample_rate, 48_000);
        let read: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
        assert_eq!(read.len(), samples.len());
        for (got, want) in read.iter().zip(samples.iter()) {
            assert!((got - want).abs() < 1e-6, "{got} vs {want}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}

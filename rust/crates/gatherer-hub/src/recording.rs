//! Per-source recording to WAV. RT-safe by construction: the audio thread
//! pushes raw captured stereo into per-source `rtrb` rings; a background
//! writer thread drains them into `hound` WAV files. No file I/O ever
//! happens on the audio thread. Mirrors FIELD's LayerWriter split.
//!
//! Recording captures the **raw** per-source input (pre gain/mute/solo/
//! invert) — those are monitoring controls, not part of the captured take.

use rtrb::{Consumer, Producer};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// ~1.36 s of stereo slack per source @ 48 k — absorbs writer scheduling.
const REC_RING_FRAMES: usize = 1 << 16;

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
    },
    Stop,
}

/// UI-side handle: arm flags + start/stop. One per engine instance (the
/// rings + writer thread are owned by the engine; dropping this on engine
/// teardown disconnects the channel and the writer thread exits).
pub struct RecorderControl {
    tx: Sender<WriterCommand>,
    pub state: Arc<RecordState>,
    pub recording: bool,
    pub last_session: Option<PathBuf>,
    /// Source indices captured in the last/current session (for loading the
    /// take back for playback + waveform display).
    pub last_armed: Vec<usize>,
    pub error: Option<String>,
}

impl RecorderControl {
    pub fn new(tx: Sender<WriterCommand>, state: Arc<RecordState>) -> Self {
        Self {
            tx,
            state,
            recording: false,
            last_session: None,
            last_armed: Vec::new(),
            error: None,
        }
    }

    pub fn toggle(&mut self, sample_rate: u32) {
        if self.recording {
            self.stop();
        } else {
            self.start(sample_rate);
        }
    }

    fn start(&mut self, sample_rate: u32) {
        let armed: Vec<usize> = (0..self.state.num_sources())
            .filter(|&s| self.state.is_armed(s))
            .collect();
        if armed.is_empty() {
            self.error = Some("Arm at least one source (R) before recording".into());
            return;
        }
        let dir = match session_dir() {
            Ok(d) => d,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        self.last_armed = armed.clone();
        self.state.set_active(true);
        if self
            .tx
            .send(WriterCommand::Start { dir: dir.clone(), armed, sample_rate })
            .is_err()
        {
            self.state.set_active(false);
            self.error = Some("recorder thread is gone".into());
            return;
        }
        self.recording = true;
        self.last_session = Some(dir);
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
    let mut writers: Vec<Option<Wav>> = (0..consumers.len()).map(|_| None).collect();
    let mut recording = false;

    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(5)) {
            Ok(WriterCommand::Start { dir, armed, sample_rate }) => {
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
                recording = true;
            }
            Ok(WriterCommand::Stop) => {
                drain(&mut consumers, &mut writers);
                // Catch a trailing block the audio thread may have pushed
                // after it observed active=false.
                std::thread::sleep(Duration::from_millis(20));
                drain(&mut consumers, &mut writers);
                finalize(&mut writers);
                recording = false;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                drain(&mut consumers, &mut writers);
                finalize(&mut writers);
                break;
            }
        }
        if recording {
            drain(&mut consumers, &mut writers);
        }
    }
}

fn drain(consumers: &mut [Consumer<f32>], writers: &mut [Option<Wav>]) {
    for (c, w) in consumers.iter_mut().zip(writers.iter_mut()) {
        if let Some(writer) = w {
            while let Ok(sample) = c.pop() {
                if writer.write_sample(sample).is_err() {
                    break;
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

/// `~/Music/Gatherer Recordings/session-<unix-secs>/`, created.
fn session_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME not set".to_string())?;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = home
        .join("Music")
        .join("Gatherer Recordings")
        .join(format!("session-{secs}"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(dir)
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

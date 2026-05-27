//! iced application shell: State / Message / update / view / subscription.
//!
//! Owns the cpal `AudioEngine` and the shared `HubParams`. Each device
//! input-channel pair is one gathered source with gain/mute/solo/invert.
//! The 30 Hz tick pulls peak meters from the audio thread. Built-in iced
//! widgets for now; FIELD's canvas meters/faders are a later polish pass.

use crate::audio::{self, AudioEngine};
use crate::measurement::{self, LufsMeasurement};
use crate::midi::{self, MidiSync, PPQN};
use crate::params::{linear_to_db, HubParams, GAIN_DB_MAX, GAIN_DB_MIN};
use crate::playback::Playback;
use crate::recording::{RecordState, RecorderControl};
use cpal::traits::{DeviceTrait, HostTrait};
use iced::widget::{
    button, canvas, checkbox, column, container, progress_bar, row, scrollable, slider, text,
    text_input, Space,
};
use iced::{
    alignment, mouse, Alignment, Color, Element, Length, Pixels, Point, Rectangle, Renderer, Size,
    Subscription, Task, Theme,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Timeline geometry constants.
const PIXELS_PER_UNIT: f32 = 100.0; // 1 bar (or 1 s without MIDI sync) = this many px
const LANE_HEIGHT: f32 = 56.0;
const RULER_HEIGHT: f32 = 22.0;
const TIMELINE_LABEL_WIDTH: f32 = 64.0;
const MIN_UNITS_VISIBLE: f32 = 16.0;
const TIMELINE_PADDING_UNITS: f32 = 2.0;
/// At any zoom, ensure the timeline content fills at least this much
/// horizontal pixels so low zoom doesn't leave dead viewport space.
const MIN_TIMELINE_PIXELS: f32 = 1100.0;

/// UI meter tick — matches FIELD's ~30 Hz diagnostics cadence.
const TICK_MS: u64 = 33;
/// Meter fall-off per tick (dB). ~60 dB/s at 30 Hz.
const METER_DECAY_DB: f32 = 2.0;
const METER_FLOOR_DB: f32 = -60.0;

#[derive(Debug, Clone)]
pub enum Message {
    InputDeviceSelected(String),
    OutputDeviceSelected(String),
    SetGainDb(usize, f32),
    SetMute(usize, bool),
    SetSolo(usize, bool),
    SetInvert(usize, bool),
    SetMasterGainDb(f32),
    SetArm(usize, bool),
    ToggleRecord,
    PlaybackPlay,
    PlaybackPause,
    PlaybackStop,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    TimeSigNumChanged(i32),
    SetPlayheadUnits(f32),
    SetTakeOffsetUnits(f32),
    CommitTakeOffset,
    ToggleSnap(bool),
    SetTargetLufs(f32),
    ExportStems,
    SetSessionName(String),
    SetLayerName(usize, String),
    Tick,
}

pub struct State {
    input_devices: Vec<String>,
    output_devices: Vec<String>,
    selected_input: Option<String>,
    selected_output: Option<String>,

    params: HubParams,
    engine: Option<AudioEngine>,
    recorder: Option<RecorderControl>,
    playback: Arc<Playback>,
    midi: Option<MidiSync>,
    num_sources: usize,
    error: Option<String>,

    // Display state, refreshed on Tick.
    source_peaks_db: Vec<f32>,
    master_peak_db: f32,
    master_gain_db: f32,
    /// Static loaded envelope per source index (populated after Stop + load).
    waveforms: HashMap<usize, Vec<f32>>,
    /// Set when recording stops; the take is loaded once the writer thread
    /// has had time to finalize the WAV files.
    pending_load_at: Option<Instant>,
    /// MIDI grid snapshot at record-start, used to align bar lines on the
    /// resulting waveforms.
    take_start_pulses: u64,
    take_start_bpm: f32,
    take_start_time_sig: u32,

    /// Timeline view state.
    zoom: f32,
    /// Beats-per-bar (numerator) — set by the user to match the DAW; MIDI
    /// Clock doesn't transmit time signature in real time.
    time_sig_num: u32,
    /// User-applied horizontal offset of the loaded take, in units (bars
    /// when MIDI synced, seconds otherwise). Updated continuously during a
    /// drag, snapped on release (unless `snap_to_grid` is off).
    take_user_offset_units: f32,
    /// If true, dragged clips snap to the nearest bar on release. Toggle
    /// off for free positioning.
    snap_to_grid: bool,

    /// Normalization target (LUFS, integrated). -14 is the streaming default.
    target_lufs: f32,
    /// Per-source LUFS measurement of the loaded take.
    lufs_results: HashMap<usize, LufsMeasurement>,
    /// Path of the most recent stems export (for the status line).
    last_export_dir: Option<PathBuf>,
    /// Last export error, if any.
    export_error: Option<String>,

    /// User-edited session name (blank → auto-named "session-<ts>" on Record).
    session_name: String,
    /// User-edited per-source layer names (used for export filenames; blank
    /// falls back to `source-NN`). Length tracks `num_sources`.
    layer_names: Vec<String>,
}

impl State {
    pub fn new() -> Self {
        let host = cpal::default_host();
        let input_devices = device_names(host.input_devices().ok());
        let output_devices = device_names(host.output_devices().ok());
        let selected_input = host.default_input_device().and_then(|d| d.name().ok());
        let selected_output = host.default_output_device().and_then(|d| d.name().ok());

        let mut state = Self {
            input_devices,
            output_devices,
            selected_input,
            selected_output,
            params: HubParams::new(0),
            engine: None,
            recorder: None,
            playback: Playback::new(),
            midi: midi::start("Gatherer Hub")
                .map_err(|e| eprintln!("gatherer-hub: MIDI sync disabled: {e}"))
                .ok(),
            num_sources: 0,
            error: None,
            source_peaks_db: Vec::new(),
            master_peak_db: METER_FLOOR_DB,
            master_gain_db: 0.0,
            waveforms: HashMap::new(),
            pending_load_at: None,
            take_start_pulses: 0,
            take_start_bpm: 0.0,
            take_start_time_sig: 4,
            zoom: 1.0,
            time_sig_num: 4,
            take_user_offset_units: 0.0,
            snap_to_grid: true,
            target_lufs: -14.0,
            lufs_results: HashMap::new(),
            last_export_dir: None,
            export_error: None,
            session_name: String::new(),
            layer_names: Vec::new(),
        };
        state.restart_engine();
        state
    }

    /// (Re)build params sized to the selected input device and start a
    /// fresh engine. Dropping the old engine first stops its streams.
    fn restart_engine(&mut self) {
        self.engine = None; // stop old streams before rebuilding params

        let in_ch = audio::input_channel_count(self.selected_input.as_deref());
        self.num_sources = in_ch / 2;
        self.params = HubParams::new(self.num_sources);
        self.source_peaks_db = vec![METER_FLOOR_DB; self.num_sources];
        self.master_peak_db = METER_FLOOR_DB;

        let (tx, rx) = mpsc::channel();
        let record_state = RecordState::new(self.num_sources);

        match AudioEngine::start(
            self.params.clone(),
            record_state.clone(),
            rx,
            self.playback.clone(),
            self.selected_input.as_deref(),
            self.selected_output.as_deref(),
        ) {
            Ok(engine) => {
                self.num_sources = engine.num_sources;
                self.source_peaks_db = vec![METER_FLOOR_DB; self.num_sources];
                // Keep existing user-typed layer names; pad/truncate to fit.
                self.layer_names.resize(self.num_sources, String::new());
                self.engine = Some(engine);
                // Replacing `recorder` drops the previous one, disconnecting
                // the old writer thread's channel so it finalizes and exits.
                self.recorder = Some(RecorderControl::new(tx, record_state));
                self.error = None;
            }
            Err(e) => {
                self.error = Some(e);
                self.recorder = None;
            }
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::InputDeviceSelected(name) => {
                self.selected_input = Some(name);
                self.restart_engine();
            }
            Message::OutputDeviceSelected(name) => {
                self.selected_output = Some(name);
                self.restart_engine();
            }
            Message::SetGainDb(i, db) => {
                if let Some(sp) = self.params.sources.get(i) {
                    sp.store_gain_db(db);
                }
            }
            Message::SetMute(i, on) => {
                if let Some(sp) = self.params.sources.get(i) {
                    sp.muted.store(on, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Message::SetSolo(i, on) => {
                if let Some(sp) = self.params.sources.get(i) {
                    sp.soloed.store(on, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Message::SetInvert(i, on) => {
                if let Some(sp) = self.params.sources.get(i) {
                    sp.invert.store(on, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Message::SetMasterGainDb(db) => {
                self.master_gain_db = db;
                self.params
                    .master_gain
                    .store(crate::params::db_to_linear(db), std::sync::atomic::Ordering::Relaxed);
            }
            Message::SetArm(i, on) => {
                // Arm is locked while recording (the UI also hides the toggle).
                if let Some(r) = self.recorder.as_ref() {
                    if !r.recording {
                        r.state.set_armed(i, on);
                    }
                }
            }
            Message::ToggleRecord => {
                let sr = self.engine.as_ref().map(|e| e.sample_rate as u32).unwrap_or(48000);
                let session_name = self.session_name.clone();
                let (was, now) = if let Some(r) = self.recorder.as_mut() {
                    let was = r.recording;
                    r.toggle(sr, &session_name);
                    (was, r.recording)
                } else {
                    (false, false)
                };
                if !was && now {
                    // Back-fill an auto-generated name into the UI so the
                    // user sees what the session is called.
                    if self.session_name.trim().is_empty() {
                        if let Some(r) = self.recorder.as_ref() {
                            if let Some(name) = r.last_session_name.as_ref() {
                                self.session_name = name.clone();
                            }
                        }
                    }
                    // Snapshot MIDI grid + time signature for bar alignment,
                    // pause playback, clear previous loaded waveforms (live
                    // preview feeds the lanes from here).
                    self.take_start_pulses = self
                        .midi
                        .as_ref()
                        .map(|m| m.state.pulses())
                        .unwrap_or(0);
                    self.take_start_bpm = self
                        .midi
                        .as_ref()
                        .map(|m| m.state.bpm())
                        .unwrap_or(0.0);
                    self.take_start_time_sig = self.time_sig_num;
                    self.take_user_offset_units = 0.0;
                    self.playback.pause();
                    self.waveforms.clear();
                }
                if was && !now {
                    // Load the take once the writer thread finalizes the WAVs.
                    self.pending_load_at = Some(Instant::now());
                }
            }
            Message::PlaybackPlay => self.playback.play(),
            Message::PlaybackPause => self.playback.pause(),
            Message::PlaybackStop => self.playback.stop(),
            Message::ZoomIn => {
                self.zoom = (self.zoom * 1.5).min(8.0);
            }
            Message::ZoomOut => {
                self.zoom = (self.zoom / 1.5).max(0.15);
            }
            Message::ZoomReset => {
                self.zoom = 1.0;
            }
            Message::TimeSigNumChanged(delta) => {
                let next = (self.time_sig_num as i32 + delta).clamp(1, 32);
                self.time_sig_num = next as u32;
            }
            Message::SetPlayheadUnits(units) => self.set_playhead_units(units),
            Message::SetTakeOffsetUnits(units) => {
                self.take_user_offset_units = units;
            }
            Message::CommitTakeOffset => {
                // Snap to the nearest bar (or second when no MIDI sync) when
                // snap is on; otherwise leave the free-positioned value.
                if self.snap_to_grid {
                    self.take_user_offset_units = self.take_user_offset_units.round();
                }
            }
            Message::ToggleSnap(on) => {
                self.snap_to_grid = on;
            }
            Message::SetTargetLufs(lufs) => {
                self.target_lufs = lufs.clamp(-40.0, 0.0);
            }
            Message::ExportStems => self.do_export_stems(),
            Message::SetSessionName(name) => {
                self.session_name = name;
            }
            Message::SetLayerName(i, name) => {
                if i >= self.layer_names.len() {
                    self.layer_names.resize(i + 1, String::new());
                }
                self.layer_names[i] = name;
            }
            Message::Tick => {
                self.refresh_meters();
                if let Some(t) = self.pending_load_at {
                    if t.elapsed() > Duration::from_millis(400) {
                        self.pending_load_at = None;
                        self.load_take();
                    }
                }
            }
        }
        Task::none()
    }

    fn refresh_meters(&mut self) {
        for (i, sp) in self.params.sources.iter().enumerate() {
            let (pl, pr) = sp.take_peaks();
            let new_db = linear_to_db(pl.max(pr));
            let cur = self.source_peaks_db.get(i).copied().unwrap_or(METER_FLOOR_DB);
            self.source_peaks_db[i] = decay(cur, new_db);
        }
        let (ml, mr) = (
            self.params.master_peak_l.swap(0.0, std::sync::atomic::Ordering::Relaxed),
            self.params.master_peak_r.swap(0.0, std::sync::atomic::Ordering::Relaxed),
        );
        self.master_peak_db = decay(self.master_peak_db, linear_to_db(ml.max(mr)));
    }

    /// Read the last recorded take's WAVs into the playback engine and build
    /// the display envelopes. Called shortly after recording stops.
    fn load_take(&mut self) {
        let Some(r) = self.recorder.as_ref() else {
            return;
        };
        let Some(session_root) = r.last_session.clone() else {
            return;
        };
        let armed = r.last_armed.clone();
        if armed.is_empty() {
            return;
        }
        // Recording WAVs live under `<session_root>/recording/`.
        let recording_dir = session_root.join("recording");
        match crate::playback::load_take(
            &recording_dir,
            &armed,
            self.take_start_pulses,
            self.take_start_bpm,
            self.take_start_time_sig,
        ) {
            Ok((data, envs)) => {
                // Per-source LUFS measurement (integrated + max short-term +
                // max momentary). Synchronous; takes ~100ms per minute of audio.
                let sr_u = self
                    .engine
                    .as_ref()
                    .map(|e| e.sample_rate as u32)
                    .unwrap_or(48_000);
                let mut measurements = HashMap::new();
                for (i, &src_idx) in armed.iter().enumerate() {
                    if let Some(samples) = data.sources.get(i) {
                        measurements
                            .insert(src_idx, measurement::measure(samples, 2, sr_u));
                    }
                }
                self.lufs_results = measurements;
                self.playback.set_take(data);
                self.waveforms = envs.into_iter().collect();
                self.take_user_offset_units = 0.0;
                self.last_export_dir = None;
                self.export_error = None;
            }
            Err(e) => self.error = Some(format!("load take: {e}")),
        }
    }

    /// Write per-source WAVs to `~/Music/Gatherer Exports/session-<ts>/`
    /// in `original/` and `normalized/` flavors. Normalized uses each
    /// source's integrated LUFS measurement to reach `target_lufs`.
    fn do_export_stems(&mut self) {
        self.export_error = None;
        let Some(d) = self.playback.snapshot() else {
            self.export_error = Some("no take loaded".into());
            return;
        };
        let Some(rec) = self.recorder.as_ref() else {
            return;
        };
        let Some(session_root) = rec.last_session.clone() else {
            self.export_error = Some("no recorded session yet".into());
            return;
        };
        let sr = self
            .engine
            .as_ref()
            .map(|e| e.sample_rate as u32)
            .unwrap_or(48_000);
        match crate::export::export_stems(
            &d,
            sr,
            &session_root,
            &rec.last_armed,
            &self.layer_names,
            &self.lufs_results,
            self.target_lufs as f64,
        ) {
            Ok(result) => {
                self.last_export_dir = Some(result.dir);
                self.export_error = None;
            }
            Err(e) => self.export_error = Some(e),
        }
    }

    /// Click on the ruler/lane → seek the loaded take to that musical
    /// position. Uses the live UI meter and the take's bpm, and accounts
    /// for the user's drag offset so clicks line up with the visual take.
    fn set_playhead_units(&self, units: f32) {
        let sr = self.engine.as_ref().map(|e| e.sample_rate).unwrap_or(48_000.0);
        let Some(d) = self.playback.snapshot() else {
            return;
        };
        let bpm = d.bpm;
        let bpb = self.time_sig_num.max(1);
        let take_start_units = if bpm > 0.0 {
            d.start_pulses as f32 / (PPQN * bpb) as f32
        } else {
            0.0
        };
        let unit_seconds = if bpm > 0.0 {
            60.0 / bpm * bpb as f32
        } else {
            1.0
        };
        // The take has been visually shifted by the user's drag — clicks must
        // align to that, not to the recorded start.
        let visual_start_units = take_start_units + self.take_user_offset_units;
        let offset_units = (units - visual_start_units).max(0.0);
        let frames = (offset_units * unit_seconds * sr) as u64;
        let clamped = frames.min(d.len_frames as u64);
        self.playback.set_position(clamped);
    }

    pub fn view(&self) -> Element<'_, Message> {
        let input_picker = pick_row(
            "Capture",
            &self.input_devices,
            &self.selected_input,
            Message::InputDeviceSelected,
        );
        let output_picker = pick_row(
            "Monitor",
            &self.output_devices,
            &self.selected_output,
            Message::OutputDeviceSelected,
        );

        let mut body = column![column![
            text("Gatherer Hub").size(28),
            text("Each stereo input pair is one gathered source. The DAW's PDC \
                  delivers them already aligned.")
                .size(12),
        ]
        .spacing(2)]
        .spacing(10);

        // ---- LEFT column: pickers + MIDI + MASTER + Meter + Zoom + Snap. ----
        let master_block = row![
            text("MASTER").size(14).width(Length::Fixed(70.0)),
            meter_bar(self.master_peak_db, 220.0),
            slider(GAIN_DB_MIN..=GAIN_DB_MAX, self.master_gain_db, Message::SetMasterGainDb)
                .step(0.5)
                .width(Length::Fixed(160.0)),
            text(format!("{:+.1} dB", self.master_gain_db)).width(Length::Fixed(70.0)),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        let meter_block = row![
            text("Meter").width(Length::Fixed(50.0)),
            button(text("\u{2212}")).on_press(Message::TimeSigNumChanged(-1)),
            text(format!("{}/4", self.time_sig_num))
                .width(Length::Fixed(40.0))
                .align_x(alignment::Horizontal::Center),
            button(text("+")).on_press(Message::TimeSigNumChanged(1)),
            text("(match your DAW)").size(11),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let zoom_block = row![
            text("Zoom").width(Length::Fixed(50.0)),
            button(text("\u{2212}")).on_press(Message::ZoomOut),
            text(format!("{:>4.0}%", self.zoom * 100.0))
                .width(Length::Fixed(56.0))
                .align_x(alignment::Horizontal::Center),
            button(text("+")).on_press(Message::ZoomIn),
            button(text("100%")).on_press(Message::ZoomReset),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let snap_block = row![
            text("Snap").width(Length::Fixed(50.0)),
            checkbox("Snap to bars on release", self.snap_to_grid)
                .on_toggle(Message::ToggleSnap),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        // ---- LEFT (extension): Normalize & Export + per-source LUFS detail. ----
        let target_row = row![
            text("Target").width(Length::Fixed(60.0)),
            slider(-30.0..=0.0, self.target_lufs, Message::SetTargetLufs)
                .step(0.5)
                .width(Length::Fixed(220.0)),
            text(format!("{:.1} LUFS", self.target_lufs)).width(Length::Fixed(90.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let export_status: String = if let Some(p) = &self.last_export_dir {
            format!("\u{2192} {}", p.display())
        } else if let Some(e) = &self.export_error {
            format!("error: {e}")
        } else {
            "writes original/ + normalized/ under ~/Music/Gatherer Exports/".into()
        };

        let export_row = row![
            button(text("Export stems")).on_press(Message::ExportStems),
            text(export_status).size(11),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let export_block = column![
            text("Normalize & Export").size(14),
            target_row,
            export_row,
        ]
        .spacing(4);

        let mut measurements_block = column![].spacing(2);
        if !self.lufs_results.is_empty() {
            measurements_block = measurements_block
                .push(text("Measurements (max over take) + normalization Δ").size(12));
            for src in 0..self.num_sources {
                if let Some(m) = self.lufs_results.get(&src) {
                    let label = self
                        .layer_names
                        .get(src)
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("Src {}", src + 1));
                    let line = if m.integrated.is_finite() {
                        let delta = self.target_lufs as f64 - m.integrated;
                        format!(
                            "{}:  I {:>6.1}   S {:>6.1}   M {:>6.1}   \u{0394} {:>+5.1} dB",
                            label,
                            m.integrated,
                            m.max_short_term,
                            m.max_momentary,
                            delta
                        )
                    } else {
                        format!("{}:  \u{2014}", label)
                    };
                    measurements_block = measurements_block.push(text(line).size(11));
                }
            }
        }

        let session_row = row![
            text("Session").width(Length::Fixed(60.0)),
            text_input("session-<auto>", &self.session_name)
                .on_input(Message::SetSessionName)
                .width(Length::Fixed(360.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let mut left_col = column![]
            .spacing(8)
            .push(input_picker)
            .push(output_picker)
            .push(session_row)
            .push(text(midi_status_line(self.midi.as_ref(), self.time_sig_num)).size(13));
        if let Some(err) = &self.error {
            left_col = left_col.push(text(format!("audio error: {err}")).size(13));
        }
        let left_col = left_col
            .push(Space::with_height(4))
            .push(master_block)
            .push(meter_block)
            .push(zoom_block)
            .push(snap_block)
            .push(Space::with_height(6))
            .push(export_block)
            .push(measurements_block)
            .width(Length::Fixed(580.0));

        // ---- RIGHT column: Sources + Record/Transport. ----
        let (recording, rec_status) = match &self.recorder {
            Some(r) if r.recording => (
                true,
                match &r.last_session {
                    Some(p) => format!("\u{25CF} REC \u{2192} {}", p.display()),
                    None => "\u{25CF} REC".to_string(),
                },
            ),
            Some(r) => (
                false,
                match &r.error {
                    Some(e) => format!("armed: {} \u{2014} {e}", r.state.armed_count()),
                    None => format!("armed: {}", r.state.armed_count()),
                },
            ),
            None => (false, "no engine".to_string()),
        };

        let sr = self.engine.as_ref().map(|e| e.sample_rate).unwrap_or(48_000.0);
        let pos_s = self.playback.position() as f32 / sr;
        let len_s = self.playback.len_frames() as f32 / sr;

        let transport_block = row![
            button(text(if recording { "Stop" } else { "Record" }))
                .on_press(Message::ToggleRecord),
            text(rec_status).size(13),
            Space::with_width(Length::Fixed(16.0)),
            button(text("\u{25B6} Play")).on_press(Message::PlaybackPlay),
            button(text("Pause")).on_press(Message::PlaybackPause),
            button(text("Stop")).on_press(Message::PlaybackStop),
            text(format!("{pos_s:.1}s / {len_s:.1}s")).size(13),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let mut right_col = column![].spacing(4).push(text("Sources").size(14));
        for i in 0..self.num_sources {
            right_col = right_col.push(self.source_row(i));
        }
        right_col = right_col.push(Space::with_height(6));
        right_col = right_col.push(transport_block);
        let right_col = right_col.width(Length::Fill);

        body = body.push(row![left_col, right_col].spacing(20));

        // Bar-based timeline (scrollable horizontally; takes positioned at
        // their actual bar offset on the DAW's grid; click to seek).
        if self.num_sources > 0 {
            body = body.push(Space::with_height(10));
            body = body.push(text("Timeline").size(14));
            body = body.push(self.timeline_section());
        }

        container(body).padding(16).width(Length::Fill).height(Length::Fill).into()
    }

    fn timeline_section(&self) -> Element<'_, Message> {
        let recording = self.recorder.as_ref().map(|r| r.recording).unwrap_or(false);
        let preview = self.recorder.as_ref().and_then(|r| r.last_preview.as_ref());
        let sr = self.engine.as_ref().map(|e| e.sample_rate).unwrap_or(48_000.0);

        // Active grid context: recording snapshot, or loaded take, else nothing.
        // We always use the LIVE UI meter for the grid so the user can correct
        // a wrong meter after the fact — the recorded `time_sig_num` is kept
        // on the take as metadata but not used for display.
        let (bpm, start_pulses, take_len_seconds) = if recording {
            let elapsed = preview
                .map(|p| {
                    p.sources
                        .iter()
                        .map(|s| {
                            let n = s.current_bucket.load(Ordering::Relaxed);
                            n as f32 * (p.samples_per_bucket as f32 / 2.0)
                                / p.sample_rate as f32
                        })
                        .fold(0.0f32, f32::max)
                })
                .unwrap_or(0.0);
            (self.take_start_bpm, self.take_start_pulses, elapsed)
        } else if self.playback.has_take() {
            self.playback
                .snapshot()
                .map(|d| (d.bpm, d.start_pulses, d.len_frames as f32 / sr))
                .unwrap_or((0.0, 0, 0.0))
        } else {
            (0.0, 0, 0.0)
        };

        let beats_per_bar = self.time_sig_num.max(1);
        // Unit = one bar when MIDI sync is up, else one second (graceful fallback).
        let unit_seconds = if bpm > 0.0 {
            60.0 / bpm * beats_per_bar as f32
        } else {
            1.0
        };
        let pixels_per_unit = PIXELS_PER_UNIT * self.zoom;
        let take_start_units = if bpm > 0.0 {
            start_pulses as f32 / (PPQN * beats_per_bar) as f32
        } else {
            0.0
        };
        let take_len_units = take_len_seconds / unit_seconds;
        // The loaded take can be dragged horizontally by the user; the live
        // preview during recording always renders at the recorded position.
        let offset_units = if recording {
            0.0
        } else {
            self.take_user_offset_units
        };
        let effective_start_units = take_start_units + offset_units;
        let total_units = (effective_start_units + take_len_units + TIMELINE_PADDING_UNITS)
            .max(MIN_UNITS_VISIBLE);
        // Zoom changes the per-unit pixel count; clamp so very low zoom
        // still fills the viewport rather than leaving dead space.
        let total_pixels = (total_units * pixels_per_unit).max(MIN_TIMELINE_PIXELS);
        let take_start_x = effective_start_units * pixels_per_unit;
        let take_end_x = (effective_start_units + take_len_units) * pixels_per_unit;
        let unit_label = if bpm > 0.0 { "bar" } else { "s" };

        let playhead_x = if !recording && self.playback.has_take() {
            let pos_s = self.playback.position() as f32 / sr;
            let units = effective_start_units + pos_s / unit_seconds;
            Some(units * pixels_per_unit)
        } else {
            None
        };

        let draggable = !recording && self.playback.has_take();

        // Left labels column: blank ruler-height spacer, then "Src N" per lane.
        let mut labels = column![].push(Space::with_height(Length::Fixed(RULER_HEIGHT)));
        for i in 0..self.num_sources {
            labels = labels.push(
                container(text(format!("Src {}", i + 1)).size(11))
                    .height(Length::Fixed(LANE_HEIGHT))
                    .padding([0, 6])
                    .align_y(alignment::Vertical::Center),
            );
        }

        // Horizontally-scrollable content: ruler on top, one lane per source.
        let mut timeline_col =
            column![].push(ruler_view(total_pixels, unit_label, pixels_per_unit));
        for i in 0..self.num_sources {
            let env: Vec<f32> = if recording {
                preview.map(|p| p.snapshot(i)).unwrap_or_default()
            } else {
                self.waveforms.get(&i).cloned().unwrap_or_default()
            };
            let (sx, ex) = if env.is_empty() {
                (0.0, 0.0)
            } else {
                (take_start_x, take_end_x)
            };
            timeline_col = timeline_col.push(lane_view(
                total_pixels,
                env,
                sx,
                ex,
                playhead_x,
                pixels_per_unit,
                offset_units,
                draggable,
            ));
        }

        row![
            labels.width(Length::Fixed(TIMELINE_LABEL_WIDTH)),
            scrollable(timeline_col)
                .direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::default(),
                ))
                .width(Length::Fill),
        ]
        .spacing(0)
        .into()
    }

    fn source_row(&self, i: usize) -> Element<'_, Message> {
        let sp = &self.params.sources[i];
        let peak_db = self.source_peaks_db.get(i).copied().unwrap_or(METER_FLOOR_DB);
        let gain_db = sp.gain_db();
        let muted = sp.is_muted();
        let soloed = sp.is_soloed();
        let inverted = sp.is_inverted();

        let recording = self.recorder.as_ref().map(|r| r.recording).unwrap_or(false);
        let armed = self
            .recorder
            .as_ref()
            .map(|r| r.state.is_armed(i))
            .unwrap_or(false);
        // Record-arm; non-interactive while recording.
        let arm = checkbox("R", armed);
        let arm = if recording {
            arm
        } else {
            arm.on_toggle(move |b| Message::SetArm(i, b))
        };

        // Integrated LUFS + Δ (gain to reach target) readouts.
        let (lufs_str, delta_str) = self
            .lufs_results
            .get(&i)
            .map(|m| {
                if m.integrated.is_finite() {
                    let delta = self.target_lufs as f64 - m.integrated;
                    (
                        format!("{:>6.1} LUFS", m.integrated),
                        format!("\u{0394} {:>+5.1} dB", delta),
                    )
                } else {
                    ("  \u{2014}    LUFS".to_string(), String::new())
                }
            })
            .unwrap_or_default();

        let layer_name = self
            .layer_names
            .get(i)
            .cloned()
            .unwrap_or_default();

        row![
            text_input(&format!("Src {}", i + 1), &layer_name)
                .on_input(move |s| Message::SetLayerName(i, s))
                .size(13)
                .width(Length::Fixed(96.0)),
            meter_bar(peak_db, 180.0),
            arm,
            checkbox("M", muted).on_toggle(move |b| Message::SetMute(i, b)),
            checkbox("S", soloed).on_toggle(move |b| Message::SetSolo(i, b)),
            checkbox("\u{00D8}", inverted).on_toggle(move |b| Message::SetInvert(i, b)),
            slider(GAIN_DB_MIN..=GAIN_DB_MAX, gain_db, move |v| Message::SetGainDb(i, v))
                .step(0.5)
                .width(Length::Fixed(140.0)),
            text(format!("{gain_db:+.1} dB")).width(Length::Fixed(60.0)),
            text(lufs_str).size(11).width(Length::Fixed(80.0)),
            text(delta_str).size(11).width(Length::Fixed(80.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        iced::time::every(Duration::from_millis(TICK_MS)).map(|_| Message::Tick)
    }
}

/// A simple meter bar (dB → 0..1 fill over the -60..0 dB range).
fn meter_bar(peak_db: f32, width: f32) -> Element<'static, Message> {
    progress_bar(METER_FLOOR_DB..=0.0, peak_db.clamp(METER_FLOOR_DB, 0.0))
        .width(Length::Fixed(width))
        .height(Length::Fixed(16.0))
        .into()
}

fn pick_row<'a>(
    label: &'a str,
    options: &'a [String],
    selected: &'a Option<String>,
    on_select: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    row![
        text(label).width(Length::Fixed(70.0)),
        iced::widget::pick_list(options.to_vec(), selected.clone(), on_select)
            .placeholder("Select device\u{2026}")
            .width(Length::Fixed(360.0)),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

fn decay(current: f32, new_db: f32) -> f32 {
    if new_db > current {
        new_db
    } else {
        (current - METER_DECAY_DB).max(METER_FLOOR_DB)
    }
}

fn device_names(devices: Option<impl Iterator<Item = cpal::Device>>) -> Vec<String> {
    devices
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

fn midi_status_line(midi: Option<&MidiSync>, time_sig_num: u32) -> String {
    let Some(m) = midi else {
        return "MIDI: unavailable".to_string();
    };
    let s = &m.state;
    if !s.connected() {
        return "MIDI: waiting \u{2014} route your DAW's MIDI clock to \"Gatherer Hub\""
            .to_string();
    }
    let bpm = s.bpm();
    let bpm_str = if bpm > 0.0 {
        format!("{bpm:>5.1} BPM")
    } else {
        "---.- BPM".to_string()
    };
    let (bar, beat) = s.bar_beat(time_sig_num);
    let xport = if s.playing() { "\u{25B6}" } else { "\u{25A0}" };
    format!("MIDI {xport}  {bpm_str}   bar {bar} : beat {beat}   meter {time_sig_num}/4")
}

/// One lane in the bar-based timeline. Coords are pixels; the take is
/// rendered between `take_start_x` and `take_end_x` (empty range = no
/// take). Owned data so the iced `Element` is `'static`.
struct Lane {
    env: Vec<f32>,
    take_start_x: f32,
    take_end_x: f32,
    playhead_x: Option<f32>,
    pixels_per_unit: f32,
    /// Current committed offset for this take (units). When the user starts
    /// a drag we anchor against this so deltas accumulate correctly.
    take_offset_units: f32,
    /// Only loaded takes can be dragged; live previews and empty lanes are
    /// click-to-seek only.
    draggable: bool,
}

/// Per-Lane drag state, persisted by iced between renders of the same
/// canvas widget instance.
#[derive(Debug, Default)]
struct LaneInteraction {
    dragging: bool,
    anchor_x: f32,
    anchor_offset_units: f32,
}

impl canvas::Program<Message> for Lane {
    type State = LaneInteraction;

    fn update(
        &self,
        state: &mut LaneInteraction,
        event: canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(pos) = cursor.position_in(bounds) {
                    let on_take = self.draggable
                        && !self.env.is_empty()
                        && pos.x >= self.take_start_x
                        && pos.x <= self.take_end_x;
                    if on_take {
                        // Begin a drag from this anchor; reset any stale state.
                        state.dragging = true;
                        state.anchor_x = pos.x;
                        state.anchor_offset_units = self.take_offset_units;
                        return (canvas::event::Status::Captured, None);
                    } else {
                        // Click elsewhere on the lane → seek the playhead.
                        state.dragging = false;
                        let units = (pos.x / self.pixels_per_unit).max(0.0);
                        return (
                            canvas::event::Status::Captured,
                            Some(Message::SetPlayheadUnits(units)),
                        );
                    }
                }
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if state.dragging {
                    if let Some(pos) = cursor.position_in(bounds) {
                        let delta_x = pos.x - state.anchor_x;
                        let delta_units = delta_x / self.pixels_per_unit;
                        let new_offset = state.anchor_offset_units + delta_units;
                        return (
                            canvas::event::Status::Captured,
                            Some(Message::SetTakeOffsetUnits(new_offset)),
                        );
                    }
                }
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.dragging {
                    state.dragging = false;
                    return (
                        canvas::event::Status::Captured,
                        Some(Message::CommitTakeOffset),
                    );
                }
            }
            _ => {}
        }
        (canvas::event::Status::Ignored, None)
    }

    fn draw(
        &self,
        _state: &LaneInteraction,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let (w, h) = (bounds.width, bounds.height);
        let mid = h / 2.0;

        // Lane background + a single horizontal separator at the bottom
        // (between this lane and the next) — no centerline through the
        // waveform.
        frame.fill_rectangle(
            Point::new(0.0, 0.0),
            bounds.size(),
            Color::from_rgb(0.10, 0.11, 0.13),
        );
        frame.fill_rectangle(
            Point::new(0.0, h - 1.0),
            Size::new(w, 1.0),
            Color::from_rgb(0.22, 0.23, 0.27),
        );

        // Bar/unit grid. Faint verticals across the lane at the current zoom.
        let grid_color = Color::from_rgb(0.30, 0.30, 0.36);
        let mut x = 0.0;
        while x <= w {
            frame.stroke(
                &canvas::Path::line(Point::new(x, 0.0), Point::new(x, h)),
                canvas::Stroke::default().with_color(grid_color).with_width(1.0),
            );
            x += self.pixels_per_unit;
        }

        // Waveform (normalized per take so quiet recordings still fill the lane).
        let n = self.env.len();
        if n > 0 && self.take_end_x > self.take_start_x {
            let take_w = self.take_end_x - self.take_start_x;
            let col_w = (take_w / n as f32).max(1.0);
            let max = self.env.iter().cloned().fold(1e-6_f32, f32::max);
            let wave = Color::from_rgb(0.45, 0.72, 1.0);
            for (i, &amp) in self.env.iter().enumerate() {
                let half = (amp / max).clamp(0.0, 1.0) * mid;
                let xp = self.take_start_x + i as f32 / n as f32 * take_w;
                let bar_h = (half * 2.0).max(1.0);
                frame.fill_rectangle(Point::new(xp, mid - half), Size::new(col_w, bar_h), wave);
            }
        }

        // Playhead (only while a loaded take is being played).
        if let Some(px) = self.playhead_x {
            if px >= 0.0 && px <= w {
                frame.stroke(
                    &canvas::Path::line(Point::new(px, 0.0), Point::new(px, h)),
                    canvas::Stroke::default()
                        .with_color(Color::from_rgb(1.0, 0.85, 0.3))
                        .with_width(1.5),
                );
            }
        }

        vec![frame.into_geometry()]
    }
}

fn lane_view(
    total_pixels: f32,
    env: Vec<f32>,
    take_start_x: f32,
    take_end_x: f32,
    playhead_x: Option<f32>,
    pixels_per_unit: f32,
    take_offset_units: f32,
    draggable: bool,
) -> Element<'static, Message> {
    canvas(Lane {
        env,
        take_start_x,
        take_end_x,
        playhead_x,
        pixels_per_unit,
        take_offset_units,
        draggable,
    })
    .width(Length::Fixed(total_pixels))
    .height(Length::Fixed(LANE_HEIGHT))
    .into()
}

/// Top-of-timeline ruler: tick + numeric label at every grid unit.
/// `unit_label` is "bar" (1-based labels) or "s" (0-based seconds).
/// Clicks emit `SetPlayheadUnits(units)` to seek the loaded take.
struct Ruler {
    unit_label: &'static str,
    pixels_per_unit: f32,
}

impl canvas::Program<Message> for Ruler {
    type State = ();

    fn update(
        &self,
        _state: &mut (),
        event: canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        if let canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event {
            if let Some(pos) = cursor.position_in(bounds) {
                let units = (pos.x / self.pixels_per_unit).max(0.0);
                return (
                    canvas::event::Status::Captured,
                    Some(Message::SetPlayheadUnits(units)),
                );
            }
        }
        (canvas::event::Status::Ignored, None)
    }

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let (w, h) = (bounds.width, bounds.height);

        frame.fill_rectangle(
            Point::new(0.0, 0.0),
            bounds.size(),
            Color::from_rgb(0.13, 0.14, 0.16),
        );
        frame.fill_rectangle(
            Point::new(0.0, h - 1.0),
            Size::new(w, 1.0),
            Color::from_rgb(0.30, 0.30, 0.34),
        );

        let tick_color = Color::from_rgb(0.45, 0.45, 0.50);
        let text_color = Color::from_rgb(0.85, 0.87, 0.90);
        let mut u: u32 = 0;
        loop {
            let x = u as f32 * self.pixels_per_unit;
            if x > w {
                break;
            }
            frame.fill_rectangle(Point::new(x, h - 6.0), Size::new(1.0, 6.0), tick_color);
            let n = if self.unit_label == "bar" { u + 1 } else { u };
            let label = canvas::Text {
                content: format!("{n}"),
                position: Point::new(x + 3.0, 2.0),
                color: text_color,
                size: Pixels(11.0),
                horizontal_alignment: alignment::Horizontal::Left,
                vertical_alignment: alignment::Vertical::Top,
                ..Default::default()
            };
            frame.fill_text(label);
            u += 1;
        }

        vec![frame.into_geometry()]
    }
}

fn ruler_view(
    total_pixels: f32,
    unit_label: &'static str,
    pixels_per_unit: f32,
) -> Element<'static, Message> {
    canvas(Ruler {
        unit_label,
        pixels_per_unit,
    })
    .width(Length::Fixed(total_pixels))
    .height(Length::Fixed(RULER_HEIGHT))
    .into()
}

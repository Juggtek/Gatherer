//! iced application shell: State / Message / update / view / subscription.
//!
//! Owns the cpal `AudioEngine` and the shared `HubParams`. Each device
//! input-channel pair is one gathered source with gain/mute/solo/invert.
//! The 30 Hz tick pulls peak meters from the audio thread. Built-in iced
//! widgets for now; FIELD's canvas meters/faders are a later polish pass.

use crate::audio::{self, AudioEngine};
use crate::midi::{self, MidiSync};
use crate::params::{linear_to_db, HubParams, GAIN_DB_MAX, GAIN_DB_MIN};
use crate::playback::Playback;
use crate::recording::{RecordState, RecorderControl};
use cpal::traits::{DeviceTrait, HostTrait};
use iced::widget::{
    button, canvas, checkbox, column, container, progress_bar, row, scrollable, slider, text,
    Space,
};
use iced::{
    mouse, Alignment, Color, Element, Length, Point, Rectangle, Renderer, Size, Subscription,
    Task, Theme,
};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// Per recorded source: (source index, peak envelope) for the waveform.
    waveforms: Vec<(usize, Vec<f32>)>,
    /// Set when recording stops; the take is loaded once the writer thread
    /// has had time to finalize the WAV files.
    pending_load_at: Option<Instant>,
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
            waveforms: Vec::new(),
            pending_load_at: None,
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
                let stopped = if let Some(r) = self.recorder.as_mut() {
                    let was = r.recording;
                    r.toggle(sr);
                    was && !r.recording
                } else {
                    false
                };
                if stopped {
                    // Load the take once the writer thread has finalized the WAVs.
                    self.pending_load_at = Some(Instant::now());
                }
            }
            Message::PlaybackPlay => self.playback.play(),
            Message::PlaybackPause => self.playback.pause(),
            Message::PlaybackStop => self.playback.stop(),
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
        let Some(dir) = r.last_session.clone() else {
            return;
        };
        let armed = r.last_armed.clone();
        if armed.is_empty() {
            return;
        }
        match crate::playback::load_take(&dir, &armed) {
            Ok((data, envs)) => {
                self.playback.set_take(data);
                self.waveforms = envs;
            }
            Err(e) => self.error = Some(format!("load take: {e}")),
        }
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

        let mut body = column![
            text("Gatherer Hub").size(28),
            text("Each stereo input pair is one gathered source. The DAW's PDC \
                  delivers them already aligned.")
                .size(12),
            input_picker,
            output_picker,
        ]
        .spacing(10);

        if let Some(err) = &self.error {
            body = body.push(text(format!("audio error: {err}")).size(13));
        }

        // MIDI sync status (route your DAW's MIDI clock to "Gatherer Hub").
        body = body.push(text(midi_status_line(self.midi.as_ref())).size(13));

        // Master row.
        body = body.push(Space::with_height(6));
        body = body.push(
            row![
                text("MASTER").size(14).width(Length::Fixed(70.0)),
                meter_bar(self.master_peak_db),
                slider(GAIN_DB_MIN..=GAIN_DB_MAX, self.master_gain_db, Message::SetMasterGainDb)
                    .step(0.5)
                    .width(Length::Fixed(160.0)),
                text(format!("{:+.1} dB", self.master_gain_db)).width(Length::Fixed(70.0)),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );

        // Recording control row.
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
        body = body.push(
            row![
                button(text(if recording { "Stop" } else { "Record" }))
                    .on_press(Message::ToggleRecord),
                text(rec_status).size(13),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );

        // Playback transport (shown once a take is loaded).
        if self.playback.has_take() {
            let sr = self.engine.as_ref().map(|e| e.sample_rate).unwrap_or(48_000.0);
            let pos_s = self.playback.position() as f32 / sr;
            let len_s = self.playback.len_frames() as f32 / sr;
            body = body.push(
                row![
                    button(text("\u{25B6} Play")).on_press(Message::PlaybackPlay),
                    button(text("Pause")).on_press(Message::PlaybackPause),
                    button(text("Stop")).on_press(Message::PlaybackStop),
                    text(format!("{pos_s:.1}s / {len_s:.1}s")).size(13),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }

        // Per-source mixer rows, then the recorded-take waveforms.
        let mut rows = column![].spacing(6);
        for i in 0..self.num_sources {
            rows = rows.push(self.source_row(i));
        }
        if !self.waveforms.is_empty() {
            rows = rows.push(Space::with_height(8));
            rows = rows.push(text("Recorded take").size(14));
            let frac = self.playback.fraction();
            for (idx, env) in &self.waveforms {
                rows = rows.push(text(format!("Src {}", idx + 1)).size(11));
                rows = rows.push(waveform_lane(env, frac));
            }
        }
        body = body.push(scrollable(rows).height(Length::Fill));

        container(body).padding(20).width(Length::Fill).height(Length::Fill).into()
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

        row![
            text(format!("Src {}", i + 1)).size(14).width(Length::Fixed(70.0)),
            meter_bar(peak_db),
            arm,
            checkbox("M", muted).on_toggle(move |b| Message::SetMute(i, b)),
            checkbox("S", soloed).on_toggle(move |b| Message::SetSolo(i, b)),
            checkbox("\u{00D8}", inverted).on_toggle(move |b| Message::SetInvert(i, b)),
            slider(GAIN_DB_MIN..=GAIN_DB_MAX, gain_db, move |v| Message::SetGainDb(i, v))
                .step(0.5)
                .width(Length::Fixed(160.0)),
            text(format!("{gain_db:+.1} dB")).width(Length::Fixed(70.0)),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        iced::time::every(Duration::from_millis(TICK_MS)).map(|_| Message::Tick)
    }
}

/// A simple meter bar (dB → 0..1 fill over the -60..0 dB range).
fn meter_bar(peak_db: f32) -> Element<'static, Message> {
    progress_bar(METER_FLOOR_DB..=0.0, peak_db.clamp(METER_FLOOR_DB, 0.0))
        .width(Length::Fixed(220.0))
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

fn midi_status_line(midi: Option<&MidiSync>) -> String {
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
    let (bar, beat) = s.bar_beat();
    let xport = if s.playing() { "\u{25B6}" } else { "\u{25A0}" };
    format!("MIDI {xport}  {bpm_str}   bar {bar} : beat {beat}")
}

/// A recorded source's waveform: peak envelope (filled) + a playhead line.
struct Waveform<'a> {
    env: &'a [f32],
    playhead: f32,
}

impl canvas::Program<Message> for Waveform<'_> {
    type State = ();

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
        let mid = h / 2.0;

        // Lane background + centerline so the waveform area is always visible.
        frame.fill_rectangle(
            Point::new(0.0, 0.0),
            bounds.size(),
            Color::from_rgb(0.10, 0.11, 0.13),
        );
        frame.fill_rectangle(
            Point::new(0.0, mid - 0.5),
            Size::new(w, 1.0),
            Color::from_rgb(0.25, 0.26, 0.30),
        );

        let n = self.env.len().max(1);
        let col_w = (w / n as f32).max(1.0);
        // Normalize to the take's loudest peak so quiet recordings still
        // fill the lane (a display scaling, not a level readout).
        let max = self.env.iter().cloned().fold(1e-6_f32, f32::max);
        let wave = Color::from_rgb(0.45, 0.72, 1.0);
        for (i, &amp) in self.env.iter().enumerate() {
            let half = (amp / max).clamp(0.0, 1.0) * mid;
            let x = i as f32 / n as f32 * w;
            let bar_h = (half * 2.0).max(1.0); // keep a 1px sliver at silence
            frame.fill_rectangle(Point::new(x, mid - half), Size::new(col_w, bar_h), wave);
        }

        let px = self.playhead.clamp(0.0, 1.0) * w;
        frame.stroke(
            &canvas::Path::line(Point::new(px, 0.0), Point::new(px, h)),
            canvas::Stroke::default()
                .with_color(Color::from_rgb(1.0, 0.85, 0.3))
                .with_width(1.5),
        );

        vec![frame.into_geometry()]
    }
}

fn waveform_lane(env: &[f32], playhead: f32) -> Element<'_, Message> {
    canvas(Waveform { env, playhead })
        .width(Length::Fill)
        .height(Length::Fixed(56.0))
        .into()
}

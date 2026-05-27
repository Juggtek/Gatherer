//! iced application shell: State / Message / update / view / subscription.
//!
//! Owns the cpal `AudioEngine` and the shared `HubParams`. Each device
//! input-channel pair is one gathered source with gain/mute/solo/invert.
//! The 30 Hz tick pulls peak meters from the audio thread. Built-in iced
//! widgets for now; FIELD's canvas meters/faders are a later polish pass.

use crate::audio::{self, AudioEngine};
use crate::params::{linear_to_db, HubParams, GAIN_DB_MAX, GAIN_DB_MIN};
use crate::recording::{RecordState, RecorderControl};
use cpal::traits::{DeviceTrait, HostTrait};
use iced::widget::{
    button, checkbox, column, container, progress_bar, row, scrollable, slider, text, Space,
};
use iced::{Alignment, Element, Length, Subscription, Task};
use std::sync::mpsc;
use std::time::Duration;

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
    num_sources: usize,
    error: Option<String>,

    // Display state, refreshed on Tick.
    source_peaks_db: Vec<f32>,
    master_peak_db: f32,
    master_gain_db: f32,
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
            num_sources: 0,
            error: None,
            source_peaks_db: Vec::new(),
            master_peak_db: METER_FLOOR_DB,
            master_gain_db: 0.0,
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
                if let Some(r) = self.recorder.as_mut() {
                    r.toggle(sr);
                }
            }
            Message::Tick => self.refresh_meters(),
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

        // Per-source rows.
        let mut rows = column![].spacing(6);
        for i in 0..self.num_sources {
            rows = rows.push(self.source_row(i));
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

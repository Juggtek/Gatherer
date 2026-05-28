//! iced application shell: State / Message / update / view / subscription.
//!
//! Owns the cpal `AudioEngine` and the shared `HubParams`. Each device
//! input-channel pair is one gathered source with gain/mute/solo/invert.
//! The 30 Hz tick pulls peak meters from the audio thread. Built-in iced
//! widgets for now; FIELD's canvas meters/faders are a later polish pass.

use crate::adaptive::{AdaptiveMixer, Mode, Mood, SlotField, SLOT_COUNT};
use crate::navigator::SectionKind;
use crate::audio::{self, AudioEngine};
use crate::measurement::{self, LufsMeasurement};
use crate::midi::{self, MidiSync, PPQN};
use crate::params::{linear_to_db, HubParams, GAIN_DB_MAX, GAIN_DB_MIN};
use crate::playback::Playback;
use crate::recording::{RecordState, RecorderControl};
use cpal::traits::{DeviceTrait, HostTrait};
use iced::widget::{
    button, canvas, checkbox, column, container, progress_bar, row, scrollable, slider, text,
    text_input, vertical_slider, Space,
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
const LANE_HEIGHT: f32 = 48.0;
const RULER_HEIGHT: f32 = 22.0;
#[allow(dead_code)] // superseded by SOURCES_COL_WIDTH
const TIMELINE_LABEL_WIDTH: f32 = 64.0;
/// Width of the sources column on the left of the timeline. Fits the
/// per-source row (name/meter/R-M-S-Ø/slider/dB/LUFS/Δ).
const SOURCES_COL_WIDTH: f32 = 700.0;
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
    SaveSession,
    LoadSession(String),
    ToggleAdaptive(bool),
    SetIntensity(f32),
    SetMood(Mood),
    SetMode(Mode),
    SetSmoothMs(f32),
    ToggleTargetCurve(bool),
    ToggleTargetCurvePopover,
    SetSlotField(usize, SlotField, f32),
    SetSlotFormula(usize, i32),
    SetTargetCurveField(Mode, SlotField, f32),
    SetTargetCurveFormula(Mode, i32),
    SetMoodWeight(Mood, usize, f32),
    SetBalancerMask(Mood, usize, f32),
    ApplySourcePreset(usize, String),
    ResetTargetCurveToPreset(Mode),
    ImportTemplate,
    ExportTemplate,
    OpenSessionFolder,
    SelectSection(SectionKind),
    AddSection(SectionKind),
    ToggleSequencePlayback,
    TriggerSequenceExit,
    // ── Phase C: region authoring ──────────────────────────────────
    /// Drop an in-region / out-region at the current playhead.
    AddInRegionAtPlayhead,
    AddOutRegionAtPlayhead,
    /// Generate beat-aligned out-regions every `exit_bars` bars across
    /// the whole section, or within the drag-selected range.
    GenerateExitRegions,
    GenerateExitRegionsInRange,
    SetExitBars(u32),
    /// Region metadata applied to new/generated regions.
    SetRegionFadeShape(f32),
    SetRegionFadePct(f32),
    SetRegionGroup(i32),
    /// Loop window (Main): set begin/end at playhead, or clear.
    SetLoopBeginAtPlayhead,
    SetLoopEndAtPlayhead,
    ClearLoop,
    /// Timeline range selection (units), set by dragging the ruler.
    SetSelectionRangeUnits(f32, f32),
    ClearSelection,
    /// Remove all in/out regions from the active section.
    ClearRegions,
    // ── Phase F: asset bundle I/O ──────────────────────────────────
    SaveAsset,
    ImportAsset,
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
    /// Last save/load result for the session status line.
    session_status: Option<String>,

    /// Adaptive mixer — programmatically writes per-source slider values
    /// based on the (TBD) supplied logic. Off by default.
    adaptive: AdaptiveMixer,
    /// Target-curve detail popover (in the control strip) — when open,
    /// expands a panel above the control strip that shows + edits the
    /// current mode's target curve.
    target_curve_popover_open: bool,

    /// "Generate exits every N bars" parameter (Phase C). Default 2 bars.
    exit_bars: u32,
    /// Region metadata applied to newly added / generated regions.
    region_fade_shape: f32, // 0..1 (0 expo, 0.5 linear, 1 log)
    region_fade_pct: f32,   // fade length scale; 1.0 = begin↔sync
    region_group: u32,
    /// Drag-selected frame range on the timeline (for "generate in range").
    selection_range: Option<(u64, u64)>,
    /// Asset bundle metadata (production code / variant / asset name) for
    /// Save-as-Asset. Authored properly in B1; for now defaults + derived
    /// from the session name on export.
    asset_meta: crate::navigator::model::AssetMeta,
    /// Active adaptive section. Recording, playback, and waveforms all
    /// target this section's `recording/<slug>/` folder. (Phase B0; the
    /// full project tree is authored in B1.)
    current_section: SectionKind,
    /// Per-section recorded-take metadata, so switching sections can
    /// reload the right audio + bar grid. Persisted into the project
    /// tree on save.
    section_takes: HashMap<SectionKind, SectionTake>,
    /// In-memory cache of decoded section audio + display envelopes.
    /// Populated on first load; subsequent tab switches are instant.
    /// Cleared when a new recording replaces the section or a new
    /// session is loaded.
    section_cache: HashMap<SectionKind, SectionCache>,
}

struct SectionCache {
    data: crate::playback::PlaybackData,
    waveforms: HashMap<usize, Vec<f32>>,
}

/// Live per-section take metadata (the bits `load_take` needs to
/// reconstruct playback + the bar grid for a section). Mirrors what a
/// `Pattern`'s `clip_source.take` stores in the persisted project.
#[derive(Debug, Clone)]
struct SectionTake {
    armed: Vec<usize>,
    start_pulses: u64,
    bpm: f32,
    time_sig: u32,
    user_offset_units: f32,
    /// Transition regions authored on top of this take's waveform.
    /// Not yet persisted to session.toml — re-author or re-generate after
    /// each session load until Phase B1 wires them through the project tree.
    in_regions: Vec<crate::navigator::model::Region>,
    out_regions: Vec<crate::navigator::model::Region>,
    /// Loop window `(begin, end)` in frames (Main section). `None` ⇒ the
    /// whole take loops.
    loop_range: Option<(u64, u64)>,
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
            session_status: None,
            adaptive: AdaptiveMixer::new(),
            target_curve_popover_open: false,
            exit_bars: 2,
            region_fade_shape: 0.5,
            region_fade_pct: 1.0,
            region_group: 0,
            selection_range: None,
            asset_meta: crate::navigator::model::AssetMeta::default(),
            current_section: SectionKind::Main,
            section_takes: HashMap::new(),
            section_cache: HashMap::new(),
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
                let slug = self.current_section.slug();
                let (was, now) = if let Some(r) = self.recorder.as_mut() {
                    let was = r.recording;
                    r.toggle(sr, &session_name, slug);
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
                    // Invalidate the cache for the section we just recorded
                    // so the next load hits disk (fresh data).
                    self.section_cache.remove(&self.current_section);
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
                self.refresh_normalization_gains();
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
            Message::SaveSession => self.do_save_session(),
            Message::LoadSession(name) => self.do_load_session(name),
            Message::ToggleAdaptive(on) => self.adaptive.set_enabled(on),
            Message::SetIntensity(v) => {
                self.adaptive.intensity = v.clamp(0.0, 1.0);
            }
            Message::SetMood(m) => self.adaptive.mood = m,
            Message::SetMode(m) => self.adaptive.mode = m,
            Message::SetSmoothMs(v) => {
                self.adaptive.smooth_ms = v.clamp(1.0, 5000.0);
            }
            Message::ToggleTargetCurve(on) => {
                self.adaptive.activate_target_curve = on;
            }
            Message::ToggleTargetCurvePopover => {
                self.target_curve_popover_open = !self.target_curve_popover_open;
            }
            Message::SetSlotField(slot, field, value) => {
                if let Some(p) = self.adaptive.slot_params.get_mut(slot) {
                    p.set_field(field, value);
                }
            }
            Message::SetSlotFormula(slot, delta) => {
                if let Some(p) = self.adaptive.slot_params.get_mut(slot) {
                    let next = (p.formula as i32 + delta).clamp(1, 9);
                    p.formula = next as u8;
                }
            }
            Message::SetTargetCurveField(mode, field, value) => {
                let idx = mode as usize;
                if let Some(p) = self.adaptive.target_curve.get_mut(idx) {
                    p.set_field(field, value);
                }
            }
            Message::SetTargetCurveFormula(mode, delta) => {
                let idx = mode as usize;
                if let Some(p) = self.adaptive.target_curve.get_mut(idx) {
                    let next = (p.formula as i32 + delta).clamp(1, 9);
                    p.formula = next as u8;
                }
            }
            Message::SetMoodWeight(mood, slot, value) => {
                if let Some(row) = self.adaptive.mood_weight.get_mut(mood as usize) {
                    if let Some(cell) = row.get_mut(slot) {
                        *cell = value.clamp(0.0, 1.0);
                    }
                }
            }
            Message::SetBalancerMask(mood, slot, value) => {
                if let Some(row) = self.adaptive.balancer_mask.get_mut(mood as usize) {
                    if let Some(cell) = row.get_mut(slot) {
                        *cell = value.clamp(0.0, 1.0);
                    }
                }
            }
            Message::ApplySourcePreset(slot, label) => {
                if let Some(&(_, preset)) = crate::adaptive::SOURCE_PRESETS
                    .iter()
                    .find(|(name, _)| *name == label)
                {
                    if let Some(p) = self.adaptive.slot_params.get_mut(slot) {
                        *p = preset;
                    }
                }
            }
            Message::ResetTargetCurveToPreset(mode) => {
                let idx = mode as usize;
                if let (Some(slot), Some(preset)) = (
                    self.adaptive.target_curve.get_mut(idx),
                    crate::adaptive::TARGET_PRESETS.get(idx),
                ) {
                    *slot = *preset;
                }
            }
            Message::ImportTemplate => {
                // Native file picker; remembers the last directory across
                // launches via `~/Music/Gatherer/.last_template_dir`.
                let mut dlg = rfd::FileDialog::new()
                    .set_title("Import adaptive-mixer template")
                    .add_filter("Template (.txt)", &["txt"]);
                if let Some(dir) = crate::template::read_last_dir() {
                    dlg = dlg.set_directory(dir);
                } else if let Some(dir) = crate::template::templates_dir() {
                    dlg = dlg.set_directory(dir);
                }
                if let Some(path) = dlg.pick_file() {
                    if let Some(parent) = path.parent() {
                        crate::template::write_last_dir(parent);
                    }
                    match crate::template::parse_file_into(&path, &mut self.adaptive) {
                        Ok(()) => {
                            self.session_status =
                                Some(format!("imported template \u{2192} {}", path.display()));
                        }
                        Err(e) => {
                            self.session_status = Some(format!("import error: {e}"));
                        }
                    }
                }
            }
            Message::ExportTemplate => {
                // Write into the session folder so the template lives next
                // to the recording/take/session.toml it was authored against.
                let name = if self.session_name.trim().is_empty() {
                    "untitled".to_string()
                } else {
                    self.session_name.trim().to_string()
                };
                match crate::session::ensure_root(&name) {
                    Ok(root) => {
                        let path = root.join(format!("{name}.txt"));
                        match crate::template::save_to(&path, &self.adaptive) {
                            Ok(p) => {
                                self.session_status =
                                    Some(format!("exported template \u{2192} {}", p.display()));
                            }
                            Err(e) => {
                                self.session_status =
                                    Some(format!("export template error: {e}"));
                            }
                        }
                    }
                    Err(e) => {
                        self.session_status = Some(format!("session folder error: {e}"));
                    }
                }
            }
            Message::OpenSessionFolder => {
                let name = if self.session_name.trim().is_empty() {
                    "untitled".to_string()
                } else {
                    self.session_name.trim().to_string()
                };
                match crate::session::ensure_root(&name) {
                    Ok(root) => {
                        // macOS `open`; ignore status (Finder may have its
                        // own complaints we can't act on). On other OSes
                        // we'd swap in `xdg-open` / `explorer`.
                        let _ = std::process::Command::new("open").arg(&root).spawn();
                        self.session_status =
                            Some(format!("opened \u{2192} {}", root.display()));
                    }
                    Err(e) => {
                        self.session_status = Some(format!("session folder error: {e}"));
                    }
                }
            }
            Message::SelectSection(kind) => {
                if kind != self.current_section {
                    // Stash the current section's drag offset before
                    // switching so it survives the round-trip.
                    if let Some(t) = self.section_takes.get_mut(&self.current_section) {
                        t.user_offset_units = self.take_user_offset_units;
                    }
                    self.current_section = kind;
                    self.load_section(kind);
                }
            }
            Message::AddSection(kind) => {
                // B0: sections are implicit (one of each). "Add" just
                // focuses the section so the user can record into it;
                // B1 makes the tree explicit + multi-instance.
                self.current_section = kind;
                self.load_section(kind);
            }
            Message::ToggleSequencePlayback => {
                if self.playback.seq_is_playing() {
                    self.playback.seq_set_play(false);
                } else {
                    match self.build_adaptive_program() {
                        Some(program) => {
                            self.playback.pause(); // stop single-take transport
                            self.playback.publish_adaptive_program(program);
                            self.playback.seq_set_play(true);
                            self.session_status =
                                Some("playing adaptive sequence".into());
                        }
                        None => {
                            self.session_status =
                                Some("record at least one section first".into());
                        }
                    }
                }
            }
            Message::TriggerSequenceExit => {
                if self.playback.seq_is_playing() {
                    self.playback.seq_request_exit();
                }
            }
            Message::SetExitBars(n) => self.exit_bars = n.max(1),
            Message::SetRegionFadeShape(v) => self.region_fade_shape = v.clamp(0.0, 1.0),
            Message::SetRegionFadePct(v) => self.region_fade_pct = v.clamp(0.0, 2.0),
            Message::SetRegionGroup(d) => {
                self.region_group = (self.region_group as i32 + d).clamp(0, 15) as u32;
            }
            Message::AddInRegionAtPlayhead => self.add_region_at_playhead(true),
            Message::AddOutRegionAtPlayhead => self.add_region_at_playhead(false),
            Message::GenerateExitRegions => {
                let len = self.current_section_len();
                if len > 0 {
                    let regions = self.generate_out_regions(0, len);
                    if let Some(t) = self.section_takes.get_mut(&self.current_section) {
                        t.out_regions = regions;
                    }
                }
            }
            Message::GenerateExitRegionsInRange => {
                if let Some((a, b)) = self.selection_range {
                    let regions = self.generate_out_regions(a, b);
                    if let Some(t) = self.section_takes.get_mut(&self.current_section) {
                        // Replace only regions inside the range; keep others.
                        t.out_regions.retain(|r| r.begin_frames < a || r.begin_frames > b);
                        t.out_regions.extend(regions);
                        t.out_regions.sort_by_key(|r| r.begin_frames);
                    }
                } else {
                    self.session_status = Some("drag a range on the ruler first".into());
                }
            }
            Message::SetLoopBeginAtPlayhead => {
                let pos = self.playback.position();
                if let Some(t) = self.section_takes.get_mut(&self.current_section) {
                    let end = t.loop_range.map(|(_, e)| e).unwrap_or(u64::MAX);
                    let end = if end <= pos { pos + 1 } else { end };
                    t.loop_range = Some((pos, end));
                }
            }
            Message::SetLoopEndAtPlayhead => {
                let pos = self.playback.position();
                if let Some(t) = self.section_takes.get_mut(&self.current_section) {
                    let begin = t.loop_range.map(|(b, _)| b).unwrap_or(0);
                    let begin = if begin >= pos { pos.saturating_sub(1) } else { begin };
                    t.loop_range = Some((begin, pos));
                }
            }
            Message::ClearLoop => {
                if let Some(t) = self.section_takes.get_mut(&self.current_section) {
                    t.loop_range = None;
                }
            }
            Message::SetSelectionRangeUnits(a, b) => {
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                let fa = self.units_to_section_frames(lo);
                let fb = self.units_to_section_frames(hi);
                self.selection_range = Some((fa, fb));
            }
            Message::ClearSelection => self.selection_range = None,
            Message::ClearRegions => {
                if let Some(t) = self.section_takes.get_mut(&self.current_section) {
                    t.in_regions.clear();
                    t.out_regions.clear();
                }
            }
            Message::SaveAsset => self.do_save_asset(),
            Message::ImportAsset => self.do_import_asset(),
            Message::Tick => {
                self.refresh_meters();
                if let Some(t) = self.pending_load_at {
                    if t.elapsed() > Duration::from_millis(400) {
                        self.pending_load_at = None;
                        self.load_take();
                    }
                }
                // Run the adaptive mixer step (no-op when disabled).
                self.adaptive.step(&self.params);
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

    /// Called shortly after recording stops: capture the just-recorded
    /// take's metadata into `section_takes[current_section]`, then load
    /// that section so it plays back.
    fn load_take(&mut self) {
        let armed = self
            .recorder
            .as_ref()
            .map(|r| r.last_armed.clone())
            .unwrap_or_default();
        if armed.is_empty() {
            return;
        }
        // Preserve any regions + loop the user authored before re-recording.
        let prev = self.section_takes.get(&self.current_section);
        let prev_in = prev.map(|t| t.in_regions.clone()).unwrap_or_default();
        let prev_out = prev.map(|t| t.out_regions.clone()).unwrap_or_default();
        let prev_loop = prev.and_then(|t| t.loop_range);
        self.section_takes.insert(
            self.current_section,
            SectionTake {
                armed,
                start_pulses: self.take_start_pulses,
                bpm: self.take_start_bpm,
                time_sig: self.take_start_time_sig,
                user_offset_units: 0.0,
                in_regions: prev_in,
                out_regions: prev_out,
                loop_range: prev_loop,
            },
        );
        self.load_section(self.current_section);
    }

    /// Build an adaptive playback program from the recorded sections:
    /// load each section's WAVs into a `SectionAudio` and derive its
    /// geometry. Regions are empty until Phase C authors them — so v1
    /// plays intro → main(loop) with hard hand-offs; authored regions
    /// later give smooth crosses + exits. Returns `None` if no section
    /// has a recorded take.
    fn build_adaptive_program(&self) -> Option<crate::playback::AdaptiveProgram> {
        use crate::playback::SectionAudio;
        use crate::sequencer::AdaptiveSection;
        let session_root = self.session_root()?;
        let mut geometry = Vec::new();
        let mut audio = Vec::new();
        for kind in SectionKind::ORDER {
            let Some(take) = self.section_takes.get(&kind) else {
                continue;
            };
            let dir = crate::recording::recording_dir(&session_root, kind.slug());
            let Ok((data, _envs)) = crate::playback::load_take(
                &dir,
                &take.armed,
                take.start_pulses,
                take.bpm,
                take.time_sig,
            ) else {
                continue;
            };
            let len = data.len_frames as u64;
            let authored_in = take.in_regions.clone();
            let authored_out = take.out_regions.clone();
            // Main loops its authored loop window (fallback: whole take).
            let loop_range = if kind == SectionKind::Main {
                Some(take.loop_range.unwrap_or((0, len)))
            } else {
                None
            };
            geometry.push(AdaptiveSection {
                kind,
                length: len,
                loop_range,
                in_regions: authored_in,
                out_regions: authored_out,
            });
            audio.push(SectionAudio {
                sources: data.sources,
                armed: data.armed,
            });
        }
        if audio.is_empty() {
            return None;
        }
        Some(crate::playback::AdaptiveProgram { geometry, audio })
    }

    // ── Phase C region authoring helpers ────────────────────────────

    fn current_section_len(&self) -> u64 {
        self.section_cache
            .get(&self.current_section)
            .map(|c| c.data.len_frames as u64)
            .unwrap_or(0)
    }

    /// Frames per bar for the active take (0 if no BPM / not playing).
    fn bar_frames(&self) -> f32 {
        let sr = self.engine.as_ref().map(|e| e.sample_rate).unwrap_or(48_000.0);
        let bpb = self.take_start_time_sig.max(1) as f32;
        if self.take_start_bpm > 0.0 {
            sr * 60.0 / self.take_start_bpm * bpb
        } else {
            0.0
        }
    }

    /// Frame of the first bar downbeat at/after the take's frame 0,
    /// accounting for the take starting mid-bar (`start_pulses`).
    fn first_downbeat_frames(&self) -> u64 {
        let bf = self.bar_frames();
        if bf <= 0.0 {
            return 0;
        }
        let bpb = self.take_start_time_sig.max(1);
        let take_start_units = self.take_start_pulses as f32 / (PPQN * bpb) as f32;
        let frac = take_start_units.fract();
        if frac < 1e-4 {
            0
        } else {
            ((1.0 - frac) * bf) as u64
        }
    }

    /// Convert a timeline position in *units* (bars) to a frame offset
    /// within the active take, accounting for the take's visual start +
    /// drag offset (mirrors `set_playhead_units`).
    fn units_to_section_frames(&self, units: f32) -> u64 {
        let sr = self.engine.as_ref().map(|e| e.sample_rate).unwrap_or(48_000.0);
        let bpm = self.take_start_bpm;
        let bpb = self.take_start_time_sig.max(1);
        let take_start_units = if bpm > 0.0 {
            self.take_start_pulses as f32 / (PPQN * bpb) as f32
        } else {
            0.0
        };
        let unit_seconds = if bpm > 0.0 { 60.0 / bpm * bpb as f32 } else { 1.0 };
        let visual_start = take_start_units + self.take_user_offset_units;
        let offset_units = (units - visual_start).max(0.0);
        (offset_units * unit_seconds * sr) as u64
    }

    /// Add an in/out region at the playhead using the current metadata
    /// (fade shape / pct / group). Fade length = one bar (or 0.5 s).
    fn add_region_at_playhead(&mut self, is_in: bool) {
        let pos = self.playback.position();
        let bf = self.bar_frames();
        let span = if bf > 0.0 { bf as u64 } else { (24_000.0_f32) as u64 };
        let (shape, pct, grp) = (self.region_fade_shape, self.region_fade_pct, self.region_group);
        if let Some(t) = self.section_takes.get_mut(&self.current_section) {
            if is_in {
                // in-region sync = end → place `end` at the playhead so the
                // entry aligns there: region [pos−span, pos].
                let begin = pos.saturating_sub(span);
                let mut r = crate::navigator::model::Region::new_in(begin, pos, pct, shape);
                r.group = grp;
                t.in_regions.push(r);
                t.in_regions.sort_by_key(|r| r.begin_frames);
            } else {
                // out-region sync = begin → place `begin` at the playhead:
                // region [pos, pos+span].
                let mut r = crate::navigator::model::Region::new_out(pos, pos + span, pct, shape);
                r.group = grp;
                t.out_regions.push(r);
                t.out_regions.sort_by_key(|r| r.begin_frames);
            }
        }
    }

    /// Beat-aligned out-regions every `exit_bars` bars within `[from,to]`
    /// frames. Region begins land on bar downbeats (accounting for the
    /// take's mid-bar start); fade window = one bar, with the current
    /// metadata.
    fn generate_out_regions(&self, from: u64, to: u64) -> Vec<crate::navigator::model::Region> {
        let bf = self.bar_frames();
        if bf <= 0.0 {
            return Vec::new();
        }
        let first = self.first_downbeat_frames();
        let interval = (bf * self.exit_bars as f32) as u64;
        if interval == 0 {
            return Vec::new();
        }
        let fade = bf as u64; // one-bar fade window
        // Find the first downbeat >= from.
        let mut pos = first;
        while pos < from {
            pos += interval;
        }
        let mut out = Vec::new();
        while pos < to && pos + 1 < self.current_section_len().max(to) {
            let mut r = crate::navigator::model::Region::new_out(
                pos,
                pos + fade,
                self.region_fade_pct,
                self.region_fade_shape,
            );
            r.group = self.region_group;
            out.push(r);
            pos += interval;
        }
        out
    }

    /// Decoded per-source audio for a section (cache hit, else disk).
    fn section_pcm(&self, kind: SectionKind) -> Option<crate::playback::PlaybackData> {
        if let Some(c) = self.section_cache.get(&kind) {
            return Some(c.data.clone());
        }
        let take = self.section_takes.get(&kind)?;
        let session_root = self.session_root()?;
        let dir = crate::recording::recording_dir(&session_root, kind.slug());
        crate::playback::load_take(&dir, &take.armed, take.start_pulses, take.bpm, take.time_sig)
            .ok()
            .map(|(data, _)| data)
    }

    /// Save the current project as a `<CODE> - <Variant>_TT/` bundle:
    /// `.ttasset` (mixer recipe) + `.wlamodel` (section graph) +
    /// `.wlabank` (Ogg-encoded section mix + per-layer stems).
    fn do_save_asset(&mut self) {
        use crate::asset::{ogg, ttasset, wlabank, wlamodel};
        use crate::navigator::model::AssetMeta;

        // Resolve production code + variant (derive from session name).
        let code = if !self.asset_meta.production_code.trim().is_empty() {
            self.asset_meta.production_code.trim().to_string()
        } else if !self.session_name.trim().is_empty() {
            self.session_name.trim().to_string()
        } else {
            self.session_status = Some("set a session name or production code first".into());
            return;
        };
        let variant = if self.asset_meta.variant_name.trim().is_empty() {
            code.clone()
        } else {
            self.asset_meta.variant_name.trim().to_string()
        };
        let meta = AssetMeta {
            production_code: code.clone(),
            variant_name: variant.clone(),
            asset_name: variant.clone(),
            asset_type: crate::navigator::model::AssetType::Music,
            description: self.asset_meta.description.clone(),
            tags: self.asset_meta.tags.clone(),
        };
        let Some(session_root) = self.session_root() else {
            self.session_status = Some("no session folder yet".into());
            return;
        };
        let bundle_dir = session_root.join(format!("{code} - {variant}_TT"));
        if let Err(e) = std::fs::create_dir_all(&bundle_dir) {
            self.session_status = Some(format!("create bundle dir: {e}"));
            return;
        }
        let sr = self.engine.as_ref().map(|e| e.sample_rate as u32).unwrap_or(48_000);
        let bank_name = format!("{code}.wlabank");

        // Encode each recorded section: a mixed clip + per-layer stems.
        let mut clips: Vec<wlabank::BankClip> = Vec::new();
        // (kind, pattern_id, len, in_regions, out_regions, layer_ids)
        let mut specs_data: Vec<(SectionKind, String, u64, Vec<crate::navigator::model::Region>, Vec<crate::navigator::model::Region>, Vec<String>)> = Vec::new();

        for kind in SectionKind::ORDER {
            let Some(pcm) = self.section_pcm(kind) else { continue };
            let Some(take) = self.section_takes.get(&kind) else { continue };
            let pid = format!("{}1", kind.pattern_letter());
            // Mixed clip = raw sum of the section's source stems.
            let mix = sum_sources(&pcm);
            match ogg::encode(&mix, 2, sr, 0.6) {
                Ok(o) => clips.push(wlabank::BankClip {
                    clip_name: format!("module_{bank_name}/{pid}"),
                    channels: 2, sample_rate: sr,
                    frame_count: pcm.len_frames as u64, ogg: o,
                }),
                Err(e) => { self.session_status = Some(format!("encode {pid}: {e}")); return; }
            }
            // Per-layer stems. Layer ids are 1-based to match the engine
            // (Atlas: m1l1..m1l7), so source index `src` → `l{src+1}`.
            let mut layer_ids = Vec::new();
            for (i, &src) in pcm.armed.iter().enumerate() {
                let lid = format!("{pid}l{}", src + 1);
                let Some(buf) = pcm.sources.get(i) else { continue };
                match ogg::encode(buf, 2, sr, 0.6) {
                    Ok(o) => clips.push(wlabank::BankClip {
                        clip_name: format!("module_{bank_name}/{lid}"),
                        channels: 2, sample_rate: sr,
                        frame_count: pcm.len_frames as u64, ogg: o,
                    }),
                    Err(e) => { self.session_status = Some(format!("encode {lid}: {e}")); return; }
                }
                layer_ids.push(lid);
            }
            specs_data.push((kind, pid, pcm.len_frames as u64,
                take.in_regions.clone(), take.out_regions.clone(), layer_ids));
        }

        if specs_data.is_empty() {
            self.session_status = Some("record at least one section before exporting".into());
            return;
        }

        let specs: Vec<wlamodel::SectionSpec<'_>> = specs_data.iter().map(|(kind, pid, len, inr, outr, lids)| {
            wlamodel::SectionSpec {
                kind: *kind, pattern_id: pid, len_frames: *len, bank_name: &bank_name,
                in_regions: inr, out_regions: outr, layer_ids: lids.clone(),
            }
        }).collect();

        let music_uuid = uuid::Uuid::new_v4().to_string();
        let page_uuid = uuid::Uuid::new_v4().to_string();
        let module_uuid = uuid::Uuid::new_v4().to_string();

        let r = (|| -> Result<(), String> {
            ttasset::write(&bundle_dir, &self.adaptive, &meta, &music_uuid, &page_uuid)?;
            wlamodel::write(&bundle_dir, &code, &module_uuid, &page_uuid, &specs, sr)?;
            wlabank::write(&clips, &bundle_dir.join(&bank_name))?;
            Ok(())
        })();
        self.asset_meta = meta;
        self.session_status = Some(match r {
            Ok(()) => format!("exported asset \u{2192} {}", bundle_dir.display()),
            Err(e) => format!("asset export error: {e}"),
        });
    }

    /// Import a `<CODE>_TT/` bundle picked via a file dialog: parse the
    /// three files, populate the mixer + asset meta + per-section takes
    /// + regions, and decode the bank's per-layer stems to WAVs so the
    /// existing playback/timeline paths work.
    fn do_import_asset(&mut self) {
        use crate::asset::{ogg, ttasset, wlabank, wlamodel};
        let mut dlg = rfd::FileDialog::new()
            .set_title("Import asset (.ttasset)")
            .add_filter("Asset (.ttasset)", &["ttasset"]);
        if let Some(dir) = crate::template::read_last_dir() {
            dlg = dlg.set_directory(dir);
        }
        let Some(ttasset_path) = dlg.pick_file() else { return };
        let dir = ttasset_path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        if let Some(p) = ttasset_path.parent() {
            crate::template::write_last_dir(p);
        }
        // Derive the <CODE> from the .ttasset filename.
        let code = ttasset_path.file_stem().and_then(|s| s.to_str()).unwrap_or("ANDD").to_string();

        // Parse .ttasset → mixer + meta.
        let mut meta = crate::navigator::model::AssetMeta::default();
        if let Err(e) = ttasset::read(&ttasset_path, &mut self.adaptive, &mut meta) {
            self.session_status = Some(format!("ttasset: {e}"));
            return;
        }
        self.asset_meta = meta;

        // Parse .wlamodel → sections.
        let model_path = dir.join(format!("{code}.wlamodel"));
        let sections = match wlamodel::read(&model_path) {
            Ok((_, _, s)) => s,
            Err(e) => { self.session_status = Some(format!("wlamodel: {e}")); return; }
        };

        // Decode .wlabank → per-layer WAVs into the session's recording dirs.
        let bank_path = dir.join(format!("{code}.wlabank"));
        let bank = match wlabank::read(&bank_path) {
            Ok(b) => b,
            Err(e) => { self.session_status = Some(format!("wlabank: {e}")); return; }
        };
        // Map clip name → decoded interleaved PCM.
        let mut clip_pcm: HashMap<String, Vec<f32>> = HashMap::new();
        for c in &bank {
            if let Some(ogg_bytes) = &c.ogg {
                if let Ok((pcm, _ch, _sr)) = ogg::decode(ogg_bytes) {
                    // clip_name = "module_<bank>/<id>" → keep just <id>.
                    let id = c.clip_name.rsplit('/').next().unwrap_or(&c.clip_name).to_string();
                    clip_pcm.insert(id, pcm);
                }
            }
        }

        // Set up a session folder to hold the decoded WAVs.
        if self.session_name.trim().is_empty() {
            self.session_name = code.clone();
        }
        let Some(session_root) = crate::session::ensure_root(&self.session_name).ok() else {
            self.session_status = Some("no session folder".into());
            return;
        };
        if let Some(rec) = self.recorder.as_mut() {
            rec.last_session = Some(session_root.clone());
            rec.last_session_name = Some(self.session_name.clone());
        }

        self.section_cache.clear();
        self.section_takes.clear();
        self.playback.clear_adaptive_program();

        for sec in &sections {
            let rec_dir = crate::recording::recording_dir(&session_root, sec.kind.slug());
            let _ = std::fs::create_dir_all(&rec_dir);
            // Write each layer stem as source-NN.wav (NN from the layer id suffix).
            let mut armed = Vec::new();
            for lid in &sec.layer_ids {
                // layer id = "<pid>l<N>" with N 1-based (Atlas convention)
                // → source index = N − 1.
                let n: usize = lid.rsplit('l').next().and_then(|n| n.parse().ok()).unwrap_or(1);
                let src = n.saturating_sub(1);
                if let Some(pcm) = clip_pcm.get(lid) {
                    let path = rec_dir.join(format!("source-{:02}.wav", src + 1));
                    if let Err(e) = write_stereo_wav(&path, pcm, 48_000) {
                        self.session_status = Some(format!("write {}: {e}", path.display()));
                        return;
                    }
                    armed.push(src);
                }
            }
            armed.sort_unstable();
            // Main's loop window comes from the wlamodel pattern loop.
            let loop_range = if sec.kind == SectionKind::Main && sec.len_frames > 0 {
                Some((0, sec.len_frames))
            } else {
                None
            };
            self.section_takes.insert(sec.kind, SectionTake {
                armed,
                start_pulses: 0,
                bpm: 0.0,
                time_sig: self.time_sig_num,
                user_offset_units: 0.0,
                in_regions: sec.in_regions.clone(),
                out_regions: sec.out_regions.clone(),
                loop_range,
            });
        }

        // Focus Main (or the first section) and load it.
        let focus = SectionKind::ORDER.into_iter()
            .find(|k| self.section_takes.contains_key(k))
            .unwrap_or(SectionKind::Main);
        self.current_section = focus;
        self.load_section(focus);
        self.session_status = Some(format!(
            "imported {} section(s) from {}", self.section_takes.len(), dir.display()
        ));
    }

    /// Resolve the session root from the recorder (last recording) or
    /// the typed session name.
    fn session_root(&self) -> Option<PathBuf> {
        if let Some(root) = self.recorder.as_ref().and_then(|r| r.last_session.clone()) {
            return Some(root);
        }
        crate::session::root_for(&self.session_name)
    }

    /// Load `kind`'s recorded take into the playback engine + display
    /// envelopes. On a cache hit (the section was already loaded this
    /// session) the switch is instant — no disk I/O. On a miss the WAVs
    /// are decoded from disk and the result is stored in the cache for
    /// the next switch. Clears playback/visuals when the section has no
    /// recorded take.
    fn load_section(&mut self, kind: SectionKind) {
        let Some(take) = self.section_takes.get(&kind).cloned() else {
            self.playback.clear();
            self.waveforms.clear();
            self.lufs_results.clear();
            self.take_user_offset_units = 0.0;
            return;
        };

        // ── cache hit ──────────────────────────────────────────────
        if let Some(cached) = self.section_cache.get(&kind) {
            self.playback.set_take(cached.data.clone());
            self.waveforms = cached.waveforms.clone();
            self.take_start_pulses = take.start_pulses;
            self.take_start_bpm = take.bpm;
            self.take_start_time_sig = take.time_sig;
            self.take_user_offset_units = take.user_offset_units;
            self.refresh_normalization_gains();
            return;
        }

        // ── cache miss: decode from disk ────────────────────────────
        let Some(session_root) = self.session_root() else {
            return;
        };
        let recording_dir = crate::recording::recording_dir(&session_root, kind.slug());
        match crate::playback::load_take(
            &recording_dir,
            &take.armed,
            take.start_pulses,
            take.bpm,
            take.time_sig,
        ) {
            Ok((data, envs)) => {
                let sr_u = self
                    .engine
                    .as_ref()
                    .map(|e| e.sample_rate as u32)
                    .unwrap_or(48_000);
                let mut measurements = HashMap::new();
                for (i, &src_idx) in take.armed.iter().enumerate() {
                    if let Some(samples) = data.sources.get(i) {
                        measurements
                            .insert(src_idx, measurement::measure(samples, 2, sr_u));
                    }
                }
                self.lufs_results = measurements;
                self.refresh_normalization_gains();
                let waveforms: HashMap<usize, Vec<f32>> = envs.into_iter().collect();
                self.waveforms = waveforms.clone();
                self.take_start_pulses = take.start_pulses;
                self.take_start_bpm = take.bpm;
                self.take_start_time_sig = take.time_sig;
                self.take_user_offset_units = take.user_offset_units;
                self.last_export_dir = None;
                self.export_error = None;
                // Populate the cache so next switch is instant.
                self.section_cache.insert(kind, SectionCache {
                    data: data.clone(),
                    waveforms,
                });
                self.playback.set_take(data);
            }
            Err(e) => self.error = Some(format!("load take: {e}")),
        }
    }

    /// Write per-source WAVs to `~/Music/Gatherer Exports/session-<ts>/`
    /// in `original/` and `normalized/` flavors. Normalized uses each
    /// source's integrated LUFS measurement to reach `target_lufs`.
    /// Publish per-source normalization gains (linear) to the audio
    /// thread. Called when measurements change (after take load) or when
    /// the user moves the Target LUFS slider.
    fn refresh_normalization_gains(&self) {
        for g in self.params.normalization_gains.iter() {
            g.store(1.0, Ordering::Relaxed);
        }
        let Some(rec) = self.recorder.as_ref() else {
            return;
        };
        for &src_idx in &rec.last_armed {
            let Some(m) = self.lufs_results.get(&src_idx) else {
                continue;
            };
            let g = measurement::normalization_gain(m.integrated, self.target_lufs as f64);
            if let Some(slot) = self.params.normalization_gains.get(src_idx) {
                slot.store(g, Ordering::Relaxed);
            }
        }
    }

    fn do_save_session(&mut self) {
        if self.session_name.trim().is_empty() {
            self.session_status = Some("type a session name first".into());
            return;
        }
        let session_root = match crate::session::ensure_root(&self.session_name) {
            Ok(p) => p,
            Err(e) => {
                self.session_status = Some(e);
                return;
            }
        };
        let sr = self
            .engine
            .as_ref()
            .map(|e| e.sample_rate as u32)
            .unwrap_or(48_000);
        // Snapshot the active section's live drag offset before
        // serialising, so it persists.
        if let Some(t) = self.section_takes.get_mut(&self.current_section) {
            t.user_offset_units = self.take_user_offset_units;
        }
        // Phase B0: persist every recorded section into the project tree.
        let takes: Vec<(SectionKind, crate::session::TakeState)> = self
            .section_takes
            .iter()
            .map(|(kind, st)| {
                (
                    *kind,
                    crate::session::TakeState {
                        armed: st.armed.clone(),
                        start_pulses: st.start_pulses,
                        bpm: st.bpm,
                        time_sig_num: st.time_sig,
                        take_user_offset_units: st.user_offset_units,
                    },
                )
            })
            .collect();
        let state = crate::session::SessionState::with_section_takes(
            self.session_name.clone(),
            sr,
            self.time_sig_num,
            self.target_lufs,
            self.zoom,
            self.snap_to_grid,
            self.layer_names.clone(),
            takes,
        );
        match crate::session::save(&session_root, &state) {
            Ok(()) => {
                self.session_status =
                    Some(format!("saved \u{2192} {}", session_root.display()));
            }
            Err(e) => self.session_status = Some(format!("save error: {e}")),
        }
    }

    fn do_load_session(&mut self, name: String) {
        let Some(session_root) = crate::session::root_for(&name) else {
            self.session_status = Some("HOME not set".into());
            return;
        };
        let sess = match crate::session::load(&session_root) {
            Ok(s) => s,
            Err(e) => {
                self.session_status = Some(format!("load error: {e}"));
                return;
            }
        };

        // Apply non-audio state.
        self.session_name = sess.session_name.clone();
        self.target_lufs = sess.target_lufs;
        self.zoom = sess.zoom.clamp(0.15, 8.0);
        self.snap_to_grid = sess.snap_to_grid;
        self.time_sig_num = sess.time_sig_num.max(1);
        let mut names = sess.layer_names.clone();
        names.resize(self.num_sources, String::new());
        self.layer_names = names;

        // Drop any adaptive program from the previous session and clear
        // the section cache (stale audio from a different session).
        self.playback.clear_adaptive_program();
        self.section_cache.clear();
        // Phase B0: repopulate the live per-section take map from the
        // loaded project tree.
        self.section_takes.clear();
        for (kind, take) in sess.section_takes() {
            self.section_takes.insert(
                kind,
                SectionTake {
                    armed: take.armed,
                    start_pulses: take.start_pulses,
                    bpm: take.bpm,
                    time_sig: take.time_sig_num,
                    user_offset_units: take.take_user_offset_units,
                    in_regions: Vec::new(),
                    out_regions: Vec::new(),
                    loop_range: None,
                },
            );
        }
        // Plant the recorder's session pointer so the section loader +
        // export resolve paths under this session's folder.
        if let Some(rec) = self.recorder.as_mut() {
            rec.last_session = Some(session_root.clone());
            rec.last_session_name = Some(sess.session_name.clone());
            if let Some(t) = self.section_takes.get(&SectionKind::Main) {
                rec.last_armed = t.armed.clone();
            }
        }
        // Focus a section that has audio: prefer Main, else the first
        // recorded one, else Main (empty).
        let focus = if self.section_takes.contains_key(&SectionKind::Main) {
            SectionKind::Main
        } else {
            SectionKind::ORDER
                .into_iter()
                .find(|k| self.section_takes.contains_key(k))
                .unwrap_or(SectionKind::Main)
        };
        self.current_section = focus;
        self.load_section(focus);
        let n = self.section_takes.len();
        self.session_status = Some(if n == 0 {
            format!("loaded (no takes) \u{2192} {}", session_root.display())
        } else {
            format!("loaded {n} section(s) \u{2192} {}", session_root.display())
        });
    }

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
        // ─── shared state ──────────────────────────────────────────────
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
        let export_status: String = if let Some(p) = &self.last_export_dir {
            format!("\u{2192} {}", p.display())
        } else if let Some(e) = &self.export_error {
            format!("export error: {e}")
        } else {
            String::new()
        };
        let sessions = crate::session::list_sessions();

        // ─── TOP STRIP: Title | Capture | Monitor | Session | MIDI | Master ───
        let top_strip = row![
            text("Gatherer Hub").size(20),
            iced::widget::pick_list(
                self.input_devices.clone(),
                self.selected_input.clone(),
                Message::InputDeviceSelected,
            )
            .placeholder("Capture\u{2026}")
            .width(Length::Fixed(200.0)),
            iced::widget::pick_list(
                self.output_devices.clone(),
                self.selected_output.clone(),
                Message::OutputDeviceSelected,
            )
            .placeholder("Monitor\u{2026}")
            .width(Length::Fixed(200.0)),
            text_input("session\u{2026}", &self.session_name)
                .on_input(Message::SetSessionName)
                .width(Length::Fixed(160.0)),
            button(text("Save")).on_press(Message::SaveSession),
            iced::widget::pick_list(sessions, Option::<String>::None, Message::LoadSession)
                .placeholder("Load\u{2026}")
                .width(Length::Fixed(140.0)),
            text(midi_status_line(self.midi.as_ref(), self.time_sig_num)).size(11),
            Space::with_width(Length::Fill),
            text("MASTER").size(12),
            meter_bar(self.master_peak_db, 140.0),
            slider(GAIN_DB_MIN..=GAIN_DB_MAX, self.master_gain_db, Message::SetMasterGainDb)
                .step(0.5)
                .width(Length::Fixed(120.0)),
            text(format!("{:+.1} dB", self.master_gain_db))
                .size(11)
                .width(Length::Fixed(50.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        // Status sub-line (errors + session_status) under the top strip.
        let mut top_status: iced::widget::Column<'_, Message> = column![].spacing(2);
        if let Some(err) = &self.error {
            top_status = top_status.push(text(format!("audio error: {err}")).size(11));
        }
        if let Some(s) = &self.session_status {
            if !s.is_empty() {
                top_status = top_status.push(text(s.clone()).size(11));
            }
        }

        // ─── ACTION STRIP: Record | Transport | Meter | Zoom | Snap | Target+Export ───
        let action_strip = row![
            button(text(if recording { "Stop Rec" } else { "Record" }))
                .on_press(Message::ToggleRecord),
            text(rec_status).size(11),
            Space::with_width(Length::Fixed(10.0)),
            button(text("\u{25B6} Play")).on_press(Message::PlaybackPlay),
            button(text("Pause")).on_press(Message::PlaybackPause),
            button(text("Stop")).on_press(Message::PlaybackStop),
            text(format!("{pos_s:>5.1}s / {len_s:>5.1}s")).size(11),
            Space::with_width(Length::Fixed(14.0)),
            text("Meter").size(11),
            button(text("\u{2212}")).on_press(Message::TimeSigNumChanged(-1)),
            text(format!("{}/4", self.time_sig_num))
                .size(11)
                .width(Length::Fixed(32.0))
                .align_x(alignment::Horizontal::Center),
            button(text("+")).on_press(Message::TimeSigNumChanged(1)),
            Space::with_width(Length::Fixed(14.0)),
            text("Zoom").size(11),
            button(text("\u{2212}")).on_press(Message::ZoomOut),
            text(format!("{:>3.0}%", self.zoom * 100.0))
                .size(11)
                .width(Length::Fixed(48.0))
                .align_x(alignment::Horizontal::Center),
            button(text("+")).on_press(Message::ZoomIn),
            button(text("100%")).on_press(Message::ZoomReset),
            Space::with_width(Length::Fixed(8.0)),
            checkbox("Snap", self.snap_to_grid).on_toggle(Message::ToggleSnap),
            Space::with_width(Length::Fill),
            // Template I/O — Max-patch 97-float .txt format. Import opens
            // a native file dialog (last dir remembered between launches
            // in `~/Music/Gatherer/.last_template_dir`); Export writes
            // `<session>/<session>.txt` next to the recording.
            button(text("Import tpl")).on_press(Message::ImportTemplate),
            button(text("Export tpl")).on_press(Message::ExportTemplate),
            Space::with_width(Length::Fixed(8.0)),
            text("Target").size(11),
            slider(-30.0..=0.0, self.target_lufs, Message::SetTargetLufs)
                .step(0.5)
                .width(Length::Fixed(140.0)),
            text(format!("{:>5.1} LUFS", self.target_lufs))
                .size(11)
                .width(Length::Fixed(70.0)),
            button(text("Export stems")).on_press(Message::ExportStems),
            button(text("Open folder")).on_press(Message::OpenSessionFolder),
            Space::with_width(Length::Fixed(8.0)),
            button(text("Save Asset")).on_press(Message::SaveAsset),
            button(text("Import Asset")).on_press(Message::ImportAsset),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        let mut action_status: iced::widget::Column<'_, Message> = column![].spacing(2);
        if !export_status.is_empty() {
            action_status = action_status.push(text(export_status).size(10));
        }

        // ─── MIXER VIEW (between top strip and playback strip): 8 columns ───
        let mixer_view = self.mixer_view();

        // ─── MAIN BODY: sources column on the LEFT, scrollable timeline on the RIGHT ───
        // Wrapped in a Fill-height container so the control strip pins to the bottom.
        let main_body = container(self.main_body_row())
            .width(Length::Fill)
            .height(Length::Fill);

        // ─── CONTROL STRIP (bottom, attached): Adaptive · Intensity · Mood · Mode · Smooth · Target Curve ───
        let control_strip = row![
            checkbox("Adaptive", self.adaptive.is_enabled())
                .on_toggle(Message::ToggleAdaptive),
            Space::with_width(Length::Fixed(10.0)),
            text("Intensity").size(11),
            slider(0.0..=1.0, self.adaptive.intensity, Message::SetIntensity)
                .step(0.001)
                .width(Length::Fixed(220.0)),
            text(format!("{:>5.3}", self.adaptive.intensity))
                .size(11)
                .width(Length::Fixed(48.0)),
            Space::with_width(Length::Fixed(10.0)),
            text("Mood").size(11),
            iced::widget::radio("Dark", Mood::Dark, Some(self.adaptive.mood), Message::SetMood),
            iced::widget::radio("Neutral", Mood::Neutral, Some(self.adaptive.mood), Message::SetMood),
            iced::widget::radio("Bright", Mood::Bright, Some(self.adaptive.mood), Message::SetMood),
            Space::with_width(Length::Fixed(10.0)),
            text("Mode").size(11),
            iced::widget::radio("Music", Mode::Music, Some(self.adaptive.mode), Message::SetMode),
            iced::widget::radio("Locals", Mode::Locals, Some(self.adaptive.mode), Message::SetMode),
            iced::widget::radio("Globals", Mode::Globals, Some(self.adaptive.mode), Message::SetMode),
            iced::widget::radio("Combat", Mode::Combat, Some(self.adaptive.mode), Message::SetMode),
            Space::with_width(Length::Fixed(10.0)),
            text("Smooth").size(11),
            slider(1.0..=2000.0, self.adaptive.smooth_ms, Message::SetSmoothMs)
                .step(1.0)
                .width(Length::Fixed(140.0)),
            text(format!("{:>4.0}ms", self.adaptive.smooth_ms))
                .size(11)
                .width(Length::Fixed(58.0)),
            Space::with_width(Length::Fixed(10.0)),
            checkbox("Target Curve", self.adaptive.activate_target_curve)
                .on_toggle(Message::ToggleTargetCurve),
            button(text(if self.target_curve_popover_open { "\u{25BC}" } else { "\u{2026}" }))
                .on_press(Message::ToggleTargetCurvePopover),
            Space::with_width(Length::Fill),
            text(format!(
                "POWER {:>5.3}  FACTOR {:>5.3}  TARGET {:>5.3}",
                self.adaptive.last_power_sum,
                self.adaptive.last_factor,
                self.adaptive.last_target
            ))
            .size(10),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        // Masks strip (M1 mood + M2 balancer matrices, mirrored from
        // `Adaptive-Mixer_1.0.0_mac.maxpat`'s "MOOD" panel) sits between
        // the timeline body and the control strip.
        let masks_strip = self.masks_strip();

        // Section tabs (Intro / Main / Outro) sit just above the
        // timeline — switching swaps the recording target + playback.
        let section_tabs = self.section_tabs();
        let region_strip = self.region_strip();

        // Body is the full layout — main_body has Length::Fill so the
        // control strip pins to the bottom of the window.
        let body: Element<'_, Message> = column![
            top_strip,
            top_status,
            mixer_view,
            action_strip,
            action_status,
            section_tabs,
            region_strip,
            main_body,
            masks_strip,
            control_strip,
        ]
        .spacing(8)
        .into();

        // The target-curve popover is a TRUE overlay (iced Stack): it
        // floats above the timeline without taking any layout space.
        // Positioned bottom-left with padding so it sits just above the
        // control strip and doesn't cover the mixer view.
        let content: Element<'_, Message> = if self.target_curve_popover_open {
            let overlay = container(self.target_curve_popover_view())
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(alignment::Horizontal::Right)
                .align_y(alignment::Vertical::Bottom)
                .padding(iced::Padding {
                    top: 0.0,
                    right: 16.0,
                    bottom: 56.0,
                    left: 0.0,
                });
            iced::widget::stack![body, overlay].into()
        } else {
            body
        };

        container(content)
            .padding(8)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// Mixer view — 8 columns, one per source. Each column has the curve
    /// visualizer (raw + compensated) ABOVE the 5 SlotParams sliders +
    /// Formula stepper. Curve is square, axes are intensity 0..1 / gain 0..1.
    fn mixer_view(&self) -> Element<'_, Message> {
        const COL_W: f32 = 184.0;
        const CURVE_SIDE: f32 = 168.0; // square
        const PARAM_LABEL_W: f32 = 36.0;
        const PARAM_SLIDER_W: f32 = 100.0;
        const PARAM_VALUE_W: f32 = 32.0;

        let intensity = self.adaptive.intensity;
        let n_slots = self.adaptive.slot_params.len();
        let mut row_el = row![].spacing(6).align_y(Alignment::Start);
        for s in 0..n_slots {
            let p = self.adaptive.slot_params[s];
            let label = self
                .layer_names
                .get(s)
                .map(|n| n.trim())
                .filter(|n| !n.is_empty())
                .map(|n| n.to_string())
                .unwrap_or_else(|| format!("Src {}", s + 1));

            let mk_param = |label: &'static str, value: f32, field: SlotField| -> Element<'_, Message> {
                row![
                    text(label).size(10).width(Length::Fixed(PARAM_LABEL_W)),
                    slider(0.0..=1.0, value, move |v| Message::SetSlotField(s, field, v))
                        .step(0.001)
                        .width(Length::Fixed(PARAM_SLIDER_W)),
                    text(format!("{value:.2}"))
                        .size(10)
                        .width(Length::Fixed(PARAM_VALUE_W)),
                ]
                .spacing(4)
                .align_y(Alignment::Center)
                .into()
            };

            let formula_row = row![
                text("F").size(10).width(Length::Fixed(PARAM_LABEL_W)),
                button(text("\u{2212}").size(10)).on_press(Message::SetSlotFormula(s, -1)),
                text(format!("{}", p.formula))
                    .size(10)
                    .width(Length::Fixed(PARAM_VALUE_W))
                    .align_x(alignment::Horizontal::Center),
                button(text("+").size(10)).on_press(Message::SetSlotFormula(s, 1)),
            ]
            .spacing(4)
            .align_y(Alignment::Center);

            let (raw_pts, comp_pts) = self.adaptive.slot_curves(s, 64);
            let curve = curve_view(raw_pts, comp_pts, intensity, CURVE_SIDE, CURVE_SIDE);

            // Per-source preset picker — verbatim from the Max patch's
            // `LAYERS` row of default messages. Picking applies all six
            // SlotParams in one shot (cleaner than nudging each slider).
            let preset_names: Vec<String> = crate::adaptive::SOURCE_PRESETS
                .iter()
                .map(|(n, _)| (*n).to_string())
                .collect();
            let preset_picker = iced::widget::pick_list(
                preset_names,
                Option::<String>::None,
                move |name| Message::ApplySourcePreset(s, name),
            )
            .placeholder("Preset\u{2026}")
            .text_size(10)
            .width(Length::Fixed(COL_W - 4.0));

            // Curve ABOVE the sliders, then 5 params + formula stepper.
            let col = column![
                text(label).size(11),
                curve,
                preset_picker,
                Space::with_height(Length::Fixed(2.0)),
                mk_param("Steep", p.steepness, SlotField::Steepness),
                mk_param("Dev", p.deviation, SlotField::Deviation),
                mk_param("Max", p.maximum, SlotField::Maximum),
                mk_param("Min", p.minimum, SlotField::Minimum),
                mk_param("Orig", p.original_level, SlotField::Original),
                formula_row,
            ]
            .spacing(2)
            .width(Length::Fixed(COL_W));

            row_el = row_el.push(col);
        }
        row_el.into()
    }

    /// Target-curve popover — shown when the user clicks the `…` button
    /// next to the Target Curve toggle. Edits the current mode's 6-param
    /// target curve and shows its shape over intensity.
    fn target_curve_popover_view(&self) -> Element<'_, Message> {
        let mode = self.adaptive.mode;
        let p = self.adaptive.target_curve[mode as usize];
        let intensity = self.adaptive.intensity;
        let pts = self.adaptive.target_curve_points(96);

        let mk_param = |label: &'static str, value: f32, field: SlotField| -> Element<'_, Message> {
            row![
                text(label).size(11).width(Length::Fixed(60.0)),
                slider(0.0..=1.0, value, move |v| Message::SetTargetCurveField(mode, field, v))
                    .step(0.001)
                    .width(Length::Fixed(180.0)),
                text(format!("{value:.3}"))
                    .size(11)
                    .width(Length::Fixed(48.0)),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .into()
        };

        let formula_row = row![
            text("Formula").size(11).width(Length::Fixed(60.0)),
            button(text("\u{2212}")).on_press(Message::SetTargetCurveFormula(mode, -1)),
            text(format!("{} / 9", p.formula))
                .size(11)
                .width(Length::Fixed(48.0))
                .align_x(alignment::Horizontal::Center),
            button(text("+")).on_press(Message::SetTargetCurveFormula(mode, 1)),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        // Square canvas, axes intensity 0..1 / gain 0..1.
        let curve = curve_view(pts, Vec::new(), intensity, 280.0, 280.0);

        container(
            row![
                column![
                    row![
                        text(format!("Target curve — {}", mode.label())).size(13),
                        Space::with_width(Length::Fill),
                        button(text("Reset to Max preset").size(10))
                            .on_press(Message::ResetTargetCurveToPreset(mode)),
                    ]
                    .align_y(Alignment::Center),
                    Space::with_height(Length::Fixed(4.0)),
                    mk_param("Steep", p.steepness, SlotField::Steepness),
                    mk_param("Dev", p.deviation, SlotField::Deviation),
                    mk_param("Max", p.maximum, SlotField::Maximum),
                    mk_param("Min", p.minimum, SlotField::Minimum),
                    mk_param("Orig", p.original_level, SlotField::Original),
                    formula_row,
                ]
                .spacing(3),
                Space::with_width(Length::Fixed(12.0)),
                curve,
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding(10)
        .style(|_t| container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.13, 0.14, 0.16))),
            border: iced::Border {
                color: Color::from_rgb(0.30, 0.30, 0.34),
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        })
        .into()
    }

    /// Masks strip — mirrors `Adaptive-Mixer_1.0.0_mac.maxpat`'s "MOOD"
    /// panel, **grouped by mood**: Dark | Neutral | Bright side by side.
    /// Each mood group is two stacked rows — M1 mood mask on top, M2
    /// balancer mask underneath — so a single mood's settings live
    /// together. A per-source total-volume column on the far right
    /// reads the live `params.sources[i].gain` (post-slew, what the
    /// audio thread sees this tick).
    /// Section tabs: Intro / Main / Outro. The active one is highlighted;
    /// a `\u{25CF}` marks sections that already hold a recorded take.
    /// "Trigger exit" + "Play sequence" are reserved for Phase BP and
    /// stay disabled (no `on_press`) until the adaptive player lands.
    fn section_tabs(&self) -> Element<'_, Message> {
        let mut row_el = row![text("Section").size(12)]
            .spacing(6)
            .align_y(Alignment::Center);
        for kind in SectionKind::ORDER {
            let recorded = self.section_takes.contains_key(&kind);
            let label = if recorded {
                format!("\u{25CF} {}", kind.label())
            } else {
                kind.label().to_string()
            };
            let mut b = button(text(label).size(12)).on_press(Message::SelectSection(kind));
            b = if kind == self.current_section {
                b.style(button::primary)
            } else {
                b.style(button::secondary)
            };
            row_el = row_el.push(b);
        }
        row_el = row_el
            .push(Space::with_width(Length::Fixed(16.0)))
            .push(
                button(text("+ Intro").size(11)).on_press(Message::AddSection(SectionKind::Intro)),
            )
            .push(button(text("+ Outro").size(11)).on_press(Message::AddSection(SectionKind::Outro)));
        // Adaptive playback (Phase BP): Play sequence runs the
        // intro→main→outro sequencer; Trigger exit defers Main → Outro
        // at the next out-region.
        let playing_seq = self.playback.seq_is_playing();
        let play_label = if playing_seq {
            "\u{25A0} Stop sequence"
        } else {
            "\u{25B6} Play sequence"
        };
        let mut trigger = button(text("Trigger exit").size(11));
        if playing_seq {
            trigger = trigger.on_press(Message::TriggerSequenceExit);
        }
        row_el = row_el
            .push(Space::with_width(Length::Fill))
            .push(trigger)
            .push(button(text(play_label).size(11)).on_press(Message::ToggleSequencePlayback));
        row_el.into()
    }

    /// Region-authoring strip (Phase C), below the section tabs. Add
    /// in/out regions at the playhead, set fade metadata, generate
    /// beat-aligned exits across the section or a selected range, and
    /// define the Main loop window.
    fn region_strip(&self) -> Element<'_, Message> {
        let has_range = self.selection_range.is_some();
        let is_main = self.current_section == SectionKind::Main;
        let loop_txt = self
            .section_takes
            .get(&self.current_section)
            .and_then(|t| t.loop_range)
            .map(|(b, e)| format!("loop {}\u{2013}{}", b, e))
            .unwrap_or_else(|| "loop —".into());

        let mut r = row![text("Regions").size(11)]
            .spacing(6)
            .align_y(Alignment::Center);
        r = r
            .push(button(text("+In").size(11)).on_press(Message::AddInRegionAtPlayhead))
            .push(button(text("+Exit").size(11)).on_press(Message::AddOutRegionAtPlayhead))
            // fade metadata applied to new/generated regions
            .push(text("Shape").size(11))
            .push(
                slider(0.0..=1.0, self.region_fade_shape, Message::SetRegionFadeShape)
                    .step(0.01)
                    .width(Length::Fixed(70.0)),
            )
            .push(text(format!("{:.2}", self.region_fade_shape)).size(10).width(Length::Fixed(28.0)))
            .push(text("Pct").size(11))
            .push(
                slider(0.0..=2.0, self.region_fade_pct, Message::SetRegionFadePct)
                    .step(0.05)
                    .width(Length::Fixed(70.0)),
            )
            .push(text(format!("{:.2}", self.region_fade_pct)).size(10).width(Length::Fixed(28.0)))
            .push(text("Grp").size(11))
            .push(button(text("\u{2212}").size(10)).on_press(Message::SetRegionGroup(-1)))
            .push(text(format!("{}", self.region_group)).size(11).width(Length::Fixed(16.0)))
            .push(button(text("+").size(10)).on_press(Message::SetRegionGroup(1)))
            // generate
            .push(Space::with_width(Length::Fixed(8.0)))
            .push(text("every").size(11))
            .push(button(text("\u{2212}").size(10)).on_press(Message::SetExitBars(self.exit_bars.saturating_sub(1))))
            .push(text(format!("{}bar", self.exit_bars)).size(11).width(Length::Fixed(38.0)))
            .push(button(text("+").size(10)).on_press(Message::SetExitBars(self.exit_bars + 1)))
            .push(button(text("Generate").size(11)).on_press(Message::GenerateExitRegions));
        // Gen-in-range only when a selection exists.
        let mut gen_range = button(text("Gen range").size(11));
        if has_range {
            gen_range = gen_range.on_press(Message::GenerateExitRegionsInRange);
        }
        r = r
            .push(gen_range)
            .push(button(text("Clear regions").size(11)).on_press(Message::ClearRegions));
        // Loop controls (Main only).
        if is_main {
            r = r
                .push(Space::with_width(Length::Fixed(10.0)))
                .push(text(loop_txt).size(10).width(Length::Fixed(110.0)))
                .push(button(text("Loop\u{2190}").size(11)).on_press(Message::SetLoopBeginAtPlayhead))
                .push(button(text("Loop\u{2192}").size(11)).on_press(Message::SetLoopEndAtPlayhead))
                .push(button(text("\u{2715}").size(11)).on_press(Message::ClearLoop));
        }
        if has_range {
            r = r
                .push(Space::with_width(Length::Fixed(8.0)))
                .push(button(text("Clear sel").size(11)).on_press(Message::ClearSelection));
        }
        r.into()
    }

    fn masks_strip(&self) -> Element<'_, Message> {
        const SLIDER_H: f32 = 48.0;
        const SLIDER_W: f32 = 14.0;
        const CELL_W: f32 = 24.0;
        const ROW_LABEL_W: f32 = 56.0; // "Mood" / "Balancer" left label
        const ROW_GAP: f32 = 3.0;

        // Column headers (L0..L7), preceded by an empty corner the width
        // of the row label so the slider columns line up vertically.
        let header_row = || -> Element<'_, Message> {
            let mut r = row![Space::with_width(Length::Fixed(ROW_LABEL_W))]
                .spacing(ROW_GAP)
                .align_y(Alignment::Center);
            for s in 0..SLOT_COUNT {
                r = r.push(
                    container(text(format!("L{s}")).size(9))
                        .width(Length::Fixed(CELL_W))
                        .align_x(alignment::Horizontal::Center),
                );
            }
            r.into()
        };

        // One slider row (8 cells) with a left-side label. `read` pulls
        // the current value; `msg` wraps (slot, v) into a Message for
        // the *fixed* mood the caller passes.
        let slider_row = |label: &'static str,
                          mood: Mood,
                          read: &dyn Fn(usize) -> f32,
                          msg: fn(Mood, usize, f32) -> Message|
         -> Element<'_, Message> {
            let row_h = SLIDER_H + 14.0; // slider + value text + spacing
            let mut r = row![
                container(text(label).size(10))
                    .width(Length::Fixed(ROW_LABEL_W))
                    .height(Length::Fixed(row_h))
                    .align_x(alignment::Horizontal::Right)
                    .align_y(alignment::Vertical::Center)
            ]
            .spacing(ROW_GAP)
            .align_y(Alignment::Center);
            for s in 0..SLOT_COUNT {
                let v = read(s);
                r = r.push(
                    column![
                        vertical_slider(0.0..=1.0, v, move |x| msg(mood, s, x))
                            .step(0.001)
                            .width(SLIDER_W)
                            .height(SLIDER_H),
                        text(format!("{v:.2}")).size(8),
                    ]
                    .spacing(2)
                    .align_x(Alignment::Center)
                    .width(Length::Fixed(CELL_W)),
                );
            }
            r.into()
        };

        // One mood group: title, header row, Mood row, Balancer row.
        let mood_group = |mood: Mood| -> Element<'_, Message> {
            let m_idx = mood as usize;
            column![
                text(mood.label()).size(12),
                header_row(),
                slider_row(
                    "Mood",
                    mood,
                    &|s| self.adaptive.mood_weight[m_idx][s],
                    Message::SetMoodWeight,
                ),
                slider_row(
                    "Bal",
                    mood,
                    &|s| self.adaptive.balancer_mask[m_idx][s],
                    Message::SetBalancerMask,
                ),
            ]
            .spacing(2)
            .into()
        };

        // Per-source total-volume column on the far right. One vertical
        // bar per slot reading the live `gain` the adaptive mixer
        // publishes. Lays out as a single row so its width matches the
        // mood groups' slider rows and the heights line up visually.
        let vol_height = SLIDER_H + 14.0;
        let mut vol_header = row![].spacing(ROW_GAP).align_y(Alignment::Center);
        for s in 0..SLOT_COUNT {
            vol_header = vol_header.push(
                container(text(format!("{s}")).size(10))
                    .width(Length::Fixed(CELL_W))
                    .align_x(alignment::Horizontal::Center),
            );
        }
        let mut vol_bars = row![].spacing(ROW_GAP).align_y(Alignment::End);
        for s in 0..SLOT_COUNT {
            let g = self
                .params
                .sources
                .get(s)
                .map(|p| p.load_gain())
                .unwrap_or(0.0);
            vol_bars = vol_bars.push(
                column![
                    canvas(VolumeBar { value: g })
                        .width(Length::Fixed(SLIDER_W))
                        .height(Length::Fixed(vol_height)),
                    text(format!("{g:.2}")).size(8),
                ]
                .spacing(2)
                .align_x(Alignment::Center)
                .width(Length::Fixed(CELL_W)),
            );
        }
        let volumes = column![text("TOTAL VOLUME").size(12), vol_header, vol_bars].spacing(2);

        row![
            mood_group(Mood::Dark),
            Space::with_width(Length::Fixed(16.0)),
            mood_group(Mood::Neutral),
            Space::with_width(Length::Fixed(16.0)),
            mood_group(Mood::Bright),
            Space::with_width(Length::Fill),
            volumes,
        ]
        .spacing(8)
        .align_y(Alignment::Start)
        .into()
    }

    /// Sources column on the left + horizontally-scrollable timeline on the right.
    fn main_body_row(&self) -> Element<'_, Message> {

        let recording = self.recorder.as_ref().map(|r| r.recording).unwrap_or(false);
        let preview = self.recorder.as_ref().and_then(|r| r.last_preview.as_ref());
        let sr = self.engine.as_ref().map(|e| e.sample_rate).unwrap_or(48_000.0);

        // Active grid context: recording snapshot, or loaded take, else nothing.
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
        let offset_units = if recording {
            0.0
        } else {
            self.take_user_offset_units
        };
        let effective_start_units = take_start_units + offset_units;
        let total_units = (effective_start_units + take_len_units + TIMELINE_PADDING_UNITS)
            .max(MIN_UNITS_VISIBLE);
        let total_pixels = (total_units * pixels_per_unit).max(MIN_TIMELINE_PIXELS);
        let take_start_x = effective_start_units * pixels_per_unit;
        let take_end_x = (effective_start_units + take_len_units) * pixels_per_unit;
        let unit_label = if bpm > 0.0 { "bar" } else { "s" };
        let playhead_x = if !recording && self.playback.has_take() {
            let pos_s = self.playback.position() as f32 / sr;
            Some((effective_start_units + pos_s / unit_seconds) * pixels_per_unit)
        } else {
            None
        };
        let draggable = !recording && self.playback.has_take();

        // Sources column on the LEFT — each row aligned with its lane.
        let mut sources_col = column![]
            .spacing(0)
            .push(Space::with_height(Length::Fixed(RULER_HEIGHT)));
        for i in 0..self.num_sources {
            sources_col = sources_col.push(
                container(self.source_row(i))
                    .height(Length::Fixed(LANE_HEIGHT))
                    .padding([0, 6])
                    .align_y(alignment::Vertical::Center),
            );
        }

        // Timeline column on the RIGHT (horizontally scrollable): ruler + lanes.
        let mut timeline_col = column![]
            .spacing(0)
            .push(ruler_view(total_pixels, unit_label, pixels_per_unit));
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
            // Region/loop/selection overlay — only on the first source
            // row (same regions apply to the whole section).
            let overlay = if !recording && i == 0 {
                if let Some(t) = self.section_takes.get(&self.current_section) {
                    let len = self.section_cache.get(&self.current_section)
                        .map(|c| c.data.len_frames as u64).unwrap_or(0);
                    RegionOverlay {
                        total_frames: len,
                        in_regions: t.in_regions.clone(),
                        out_regions: t.out_regions.clone(),
                        loop_range: t.loop_range,
                        selection: self.selection_range,
                    }
                } else {
                    RegionOverlay::default()
                }
            } else {
                RegionOverlay::default()
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
                overlay,
            ));
        }

        row![
            sources_col.width(Length::Fixed(SOURCES_COL_WIDTH)),
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
                .size(12)
                .width(Length::Fixed(80.0)),
            meter_bar(peak_db, 130.0),
            arm,
            checkbox("M", muted).on_toggle(move |b| Message::SetMute(i, b)),
            checkbox("S", soloed).on_toggle(move |b| Message::SetSolo(i, b)),
            checkbox("\u{00D8}", inverted).on_toggle(move |b| Message::SetInvert(i, b)),
            slider(GAIN_DB_MIN..=GAIN_DB_MAX, gain_db, move |v| Message::SetGainDb(i, v))
                .step(0.5)
                .width(Length::Fixed(100.0)),
            text(format!("{gain_db:+.1} dB")).size(11).width(Length::Fixed(50.0)),
            text(lufs_str).size(11).width(Length::Fixed(70.0)),
            text(delta_str).size(11).width(Length::Fixed(70.0)),
        ]
        .spacing(6)
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

#[allow(dead_code)] // ex-helper from the old layout; kept for future detail panes
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
    /// Region/loop/selection overlay data (only populated on the lane
    /// that shows the overlay; empty elsewhere).
    overlay: RegionOverlay,
}

/// Region + loop + selection overlay data for a lane. Frame-based;
/// mapped to pixels against `[take_start_x, take_end_x]`.
#[derive(Default, Clone)]
struct RegionOverlay {
    total_frames: u64,
    in_regions: Vec<crate::navigator::model::Region>,
    out_regions: Vec<crate::navigator::model::Region>,
    loop_range: Option<(u64, u64)>,
    selection: Option<(u64, u64)>,
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

        // Waveform — normalized and rendered at 1-px column resolution so the
        // display is crisp at any zoom and for any envelope size.
        // Each pixel column samples the max peak from all envelope buckets that
        // fall within that pixel's time range, giving correct behaviour when the
        // take spans thousands of pixels (many buckets per pixel) or only a few
        // hundred pixels (sub-pixel buckets) by taking the peak either way.
        let n = self.env.len();
        if n > 0 && self.take_end_x > self.take_start_x {
            let take_w = self.take_end_x - self.take_start_x;
            let max = self.env.iter().cloned().fold(1e-6_f32, f32::max);
            let wave = Color::from_rgb(0.45, 0.72, 1.0);
            let px_start = self.take_start_x.max(0.0) as usize;
            let px_end = (self.take_end_x.min(w)) as usize;
            for px in px_start..px_end {
                // Map this pixel column to a range of envelope buckets.
                let t0 = (px as f32 - self.take_start_x) / take_w;
                let t1 = (px as f32 + 1.0 - self.take_start_x) / take_w;
                let b0 = ((t0 * n as f32) as usize).min(n - 1);
                let b1 = ((t1 * n as f32).ceil() as usize).clamp(b0 + 1, n);
                let peak = self.env[b0..b1].iter().cloned().fold(0.0f32, f32::max);
                let half = (peak / max).clamp(0.0, 1.0) * mid;
                let bar_h = (half * 2.0).max(1.0);
                frame.fill_rectangle(
                    Point::new(px as f32, mid - half),
                    Size::new(1.0, bar_h),
                    wave,
                );
            }
        }

        // Region / loop / selection overlays. Fades are drawn as the
        // actual gain curve (filled under the ramp) per the canonical
        // Region geometry; the sync point gets a bright vertical line.
        let ov = &self.overlay;
        if ov.total_frames > 0 && self.take_end_x > self.take_start_x {
            let take_w = self.take_end_x - self.take_start_x;
            let frames = ov.total_frames as f32;
            let region_x = |f: u64| self.take_start_x + (f as f32 / frames) * take_w;

            // Selection band (under everything else).
            if let Some((a, b)) = ov.selection {
                let x0 = region_x(a).clamp(0.0, w);
                let x1 = region_x(b).clamp(0.0, w);
                if x1 > x0 {
                    frame.fill_rectangle(
                        Point::new(x0, 0.0),
                        Size::new(x1 - x0, h),
                        Color { r: 0.5, g: 0.5, b: 0.9, a: 0.18 },
                    );
                }
            }

            // Loop window (Main): bracket lines at begin/end.
            if let Some((b, e)) = ov.loop_range {
                let xb = region_x(b).clamp(0.0, w);
                let xe = region_x(e).clamp(0.0, w);
                let loop_col = Color { r: 0.55, g: 0.85, b: 1.0, a: 0.9 };
                frame.fill_rectangle(Point::new(xb, 0.0), Size::new(2.0, h), loop_col);
                frame.fill_rectangle(Point::new(xe - 2.0, 0.0), Size::new(2.0, h), loop_col);
                frame.fill_rectangle(
                    Point::new(xb, 0.0),
                    Size::new((xe - xb).max(0.0), h),
                    Color { r: 0.40, g: 0.70, b: 1.0, a: 0.06 },
                );
            }

            // Draw a fade region's gain curve as a filled polyline.
            let mut draw_fade = |r: &crate::navigator::model::Region, is_in: bool, fill: Color, line: Color| {
                let (fs, fe) = if is_in { r.fade_span_in() } else { r.fade_span_out() };
                let span_px = (region_x(fe) - region_x(fs)).abs().max(1.0);
                let steps = (span_px as usize).clamp(2, 256);
                let mut pts: Vec<Point> = Vec::with_capacity(steps + 2);
                for i in 0..=steps {
                    let f = fs + ((fe - fs) * i as u64) / steps as u64;
                    let g = if is_in { r.gain_as_in(f) } else { r.gain_as_out(f) };
                    let x = region_x(f);
                    let y = h - g.clamp(0.0, 1.0) * h;
                    pts.push(Point::new(x, y));
                }
                // Filled area under the curve.
                if pts.len() >= 2 {
                    let area = canvas::Path::new(|p| {
                        p.move_to(Point::new(pts[0].x, h));
                        for pt in &pts {
                            p.line_to(*pt);
                        }
                        p.line_to(Point::new(pts[pts.len() - 1].x, h));
                        p.close();
                    });
                    frame.fill(&area, fill);
                    let curve = canvas::Path::new(|p| {
                        p.move_to(pts[0]);
                        for pt in &pts[1..] {
                            p.line_to(*pt);
                        }
                    });
                    frame.stroke(&curve, canvas::Stroke::default().with_color(line).with_width(1.5));
                }
                // Sync line.
                let sx = region_x(r.sync_frames).clamp(0.0, w);
                frame.fill_rectangle(Point::new(sx, 0.0), Size::new(1.0, h), line);
            };

            for r in &ov.in_regions {
                draw_fade(
                    r, true,
                    Color { r: 0.20, g: 0.75, b: 0.55, a: 0.22 },
                    Color { r: 0.30, g: 0.95, b: 0.70, a: 0.95 },
                );
            }
            for r in &ov.out_regions {
                draw_fade(
                    r, false,
                    Color { r: 0.95, g: 0.65, b: 0.10, a: 0.22 },
                    Color { r: 1.0, g: 0.80, b: 0.25, a: 0.95 },
                );
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

/// Sum a take's per-source interleaved-stereo buffers into one stereo mix.
fn sum_sources(data: &crate::playback::PlaybackData) -> Vec<f32> {
    let n = data.len_frames * 2;
    let mut mix = vec![0.0f32; n];
    for src in &data.sources {
        for (i, &s) in src.iter().enumerate() {
            if i < n {
                mix[i] += s;
            }
        }
    }
    mix
}

/// Write interleaved-stereo f32 PCM to a 32-bit-float stereo WAV.
fn write_stereo_wav(path: &std::path::Path, interleaved: &[f32], sample_rate: u32) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec)
        .map_err(|e| format!("create wav: {e}"))?;
    for &s in interleaved {
        w.write_sample(s).map_err(|e| format!("write sample: {e}"))?;
    }
    w.finalize().map_err(|e| format!("finalize wav: {e}"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn lane_view(
    total_pixels: f32,
    env: Vec<f32>,
    take_start_x: f32,
    take_end_x: f32,
    playhead_x: Option<f32>,
    pixels_per_unit: f32,
    take_offset_units: f32,
    draggable: bool,
    overlay: RegionOverlay,
) -> Element<'static, Message> {
    canvas(Lane {
        env,
        take_start_x,
        take_end_x,
        playhead_x,
        pixels_per_unit,
        take_offset_units,
        draggable,
        overlay,
    })
    .width(Length::Fixed(total_pixels))
    .height(Length::Fixed(LANE_HEIGHT))
    .into()
}

/// Top-of-timeline ruler: tick + numeric label at every grid unit.
/// `unit_label` is "bar" (1-based labels) or "s" (0-based seconds).
/// Click seeks (`SetPlayheadUnits`); click-drag selects a range
/// (`SetSelectionRangeUnits`) for region generation.
struct Ruler {
    unit_label: &'static str,
    pixels_per_unit: f32,
}

/// Persisted ruler drag state (anchor of the in-progress selection).
#[derive(Debug, Default)]
struct RulerInteraction {
    anchor_units: Option<f32>,
    moved: bool,
}

impl canvas::Program<Message> for Ruler {
    type State = RulerInteraction;

    fn update(
        &self,
        state: &mut RulerInteraction,
        event: canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(pos) = cursor.position_in(bounds) {
                    state.anchor_units = Some((pos.x / self.pixels_per_unit).max(0.0));
                    state.moved = false;
                    return (canvas::event::Status::Captured, None);
                }
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if let (Some(anchor), Some(pos)) = (state.anchor_units, cursor.position_in(bounds)) {
                    let cur = (pos.x / self.pixels_per_unit).max(0.0);
                    if (cur - anchor).abs() * self.pixels_per_unit > 3.0 {
                        state.moved = true;
                        return (
                            canvas::event::Status::Captured,
                            Some(Message::SetSelectionRangeUnits(anchor, cur)),
                        );
                    }
                }
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if let Some(anchor) = state.anchor_units.take() {
                    // No drag → treat as a seek click.
                    if !state.moved {
                        return (
                            canvas::event::Status::Captured,
                            Some(Message::SetPlayheadUnits(anchor)),
                        );
                    }
                    state.moved = false;
                }
            }
            _ => {}
        }
        (canvas::event::Status::Ignored, None)
    }

    fn draw(
        &self,
        _state: &RulerInteraction,
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

/// Curve visualizer for the mixer: shows the raw "set" curve (blue) and
/// the "compensated" curve after target-curve normalization (orange), with
/// a vertical marker at the current intensity. Pass `comp_pts = vec![]`
/// to draw only the raw curve (used by the target-curve popover).
struct CurveDisplay {
    raw_pts: Vec<f32>,
    comp_pts: Vec<f32>,
    intensity: f32,
}

impl canvas::Program<Message> for CurveDisplay {
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

        // Background + border.
        frame.fill_rectangle(
            Point::new(0.0, 0.0),
            bounds.size(),
            Color::from_rgb(0.09, 0.10, 0.12),
        );
        let grid = Color::from_rgb(0.22, 0.23, 0.27);
        // Horizontal grid at 0.25, 0.5, 0.75 (of 1.0).
        for f in [0.25_f32, 0.5, 0.75] {
            let y = h - f * h;
            frame.fill_rectangle(Point::new(0.0, y), Size::new(w, 1.0), grid);
        }
        // Vertical grid at 0.25, 0.5, 0.75 of intensity.
        for f in [0.25_f32, 0.5, 0.75] {
            let x = f * w;
            frame.fill_rectangle(Point::new(x, 0.0), Size::new(1.0, h), grid);
        }

        // y-axis fixed at 0..1; any compensated value above 1 clips at the
        // top edge (visible as a flat line riding the top).
        let draw_polyline = |frame: &mut canvas::Frame, pts: &[f32], color: Color| {
            let n = pts.len();
            if n < 2 {
                return;
            }
            let path = canvas::Path::new(|b| {
                for (i, &v) in pts.iter().enumerate() {
                    let x = i as f32 / (n - 1) as f32 * w;
                    let y = h - v.clamp(0.0, 1.0) * h;
                    if i == 0 {
                        b.move_to(Point::new(x, y));
                    } else {
                        b.line_to(Point::new(x, y));
                    }
                }
            });
            frame.stroke(
                &path,
                canvas::Stroke::default().with_color(color).with_width(1.5),
            );
        };

        // Raw "set" curve in blue, compensated in orange.
        draw_polyline(&mut frame, &self.raw_pts, Color::from_rgb(0.45, 0.72, 1.0));
        if !self.comp_pts.is_empty() {
            draw_polyline(&mut frame, &self.comp_pts, Color::from_rgb(1.0, 0.66, 0.30));
        }

        // Intensity marker (vertical line).
        let ix = self.intensity.clamp(0.0, 1.0) * w;
        frame.stroke(
            &canvas::Path::line(Point::new(ix, 0.0), Point::new(ix, h)),
            canvas::Stroke::default()
                .with_color(Color::from_rgb(1.0, 0.95, 0.55))
                .with_width(1.0),
        );

        vec![frame.into_geometry()]
    }
}

/// Tiny vertical bar showing a `0..1`-ish gain value. Bottom-up fill in
/// cyan; anything above 1.0 paints a thin red cap so over-budget slots
/// are visible at a glance.
struct VolumeBar {
    value: f32,
}

impl canvas::Program<Message> for VolumeBar {
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

        // Background.
        frame.fill_rectangle(
            Point::new(0.0, 0.0),
            bounds.size(),
            Color::from_rgb(0.09, 0.10, 0.12),
        );

        let v = self.value.max(0.0);
        let fill = v.min(1.0);
        let fill_h = fill * h;
        // Cyan fill rises from the bottom.
        frame.fill_rectangle(
            Point::new(0.0, h - fill_h),
            Size::new(w, fill_h),
            Color::from_rgb(0.35, 0.78, 1.0),
        );
        // Over-1.0 cap line at the top.
        if v > 1.0 {
            frame.fill_rectangle(
                Point::new(0.0, 0.0),
                Size::new(w, 2.0),
                Color::from_rgb(1.0, 0.35, 0.30),
            );
        }

        vec![frame.into_geometry()]
    }
}

fn curve_view(
    raw_pts: Vec<f32>,
    comp_pts: Vec<f32>,
    intensity: f32,
    width: f32,
    height: f32,
) -> Element<'static, Message> {
    canvas(CurveDisplay {
        raw_pts,
        comp_pts,
        intensity,
    })
    .width(Length::Fixed(width))
    .height(Length::Fixed(height))
    .into()
}

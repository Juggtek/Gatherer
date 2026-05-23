#include "PluginEditor.h"

#include "undo/MixCommands.h"
#include "undo/RecordingCommands.h"

#include <chrono>
#include <memory>

namespace {
constexpr int kBadgeSize = 14;
}

HubEditor::HubEditor(HubProcessor& p)
    : juce::AudioProcessorEditor(&p), processor_(p)
{
    title_.setText("Gatherer Hub", juce::dontSendNotification);
    title_.setFont(juce::FontOptions(20.0f));
    title_.setJustificationType(juce::Justification::centred);
    addAndMakeVisible(title_);

    status_.setText("attaching...", juce::dontSendNotification);
    status_.setFont(juce::FontOptions(14.0f));
    status_.setJustificationType(juce::Justification::centredLeft);
    addAndMakeVisible(status_);

    include_input_toggle_.setTooltip(
        "OFF: the hub discards whatever audio its track receives as input, "
        "outputting only the satellite mix. Use this when the hub sits on a "
        "parent group/bus track (recommended).\n\n"
        "ON: the hub keeps its track input and sums the satellite mix on top. "
        "Use this for live monitoring (mic input + sats) or when running as the "
        "Standalone app and feeding system audio into the hub.");
    include_input_toggle_.setToggleState(processor_.isIncludeTrackInput(),
                                          juce::dontSendNotification);
    include_input_toggle_.onClick = [this] {
        processor_.setIncludeTrackInput(include_input_toggle_.getToggleState());
    };
    addAndMakeVisible(include_input_toggle_);

    pad_silence_toggle_.setTooltip(
        "ON (default): each recorded WAV starts at the DAW play position, with "
        "leading silence padded for sats whose host gates processBlock by clip "
        "presence (e.g. Bitwig). All lanes' x=0 line up with the DAW transport.\n\n"
        "OFF: each WAV starts at its sat's actual first written sample. Tighter "
        "files but lanes anchor to whenever each sat began producing audio.");
    pad_silence_toggle_.setToggleState(processor_.isPadSilenceInRecord(),
                                        juce::dontSendNotification);
    pad_silence_toggle_.onClick = [this] {
        processor_.setPadSilenceInRecord(pad_silence_toggle_.getToggleState());
    };
    addAndMakeVisible(pad_silence_toggle_);

    master_record_button_.setColour(juce::TextButton::buttonColourId,
                                     juce::Colours::black.withAlpha(0.4f));
    master_record_button_.setColour(juce::TextButton::textColourOffId,
                                     juce::Colours::white);
    master_record_button_.setTooltip(
        "Start / stop recording. All record-armed layers (R button on each row) "
        "are captured to a timestamped folder in ~/Documents/Gatherer Recordings/.");
    master_record_button_.onClick = [this] {
        if (processor_.isArmedOrRecording()) processor_.stopRecording();
        else                                 processor_.startRecording();
    };
    addAndMakeVisible(master_record_button_);

    record_status_.setText("", juce::dontSendNotification);
    record_status_.setFont(juce::FontOptions(11.0f));
    record_status_.setJustificationType(juce::Justification::centredLeft);
    record_status_.setColour(juce::Label::textColourId,
                              juce::Colours::white.withAlpha(0.6f));
    addAndMakeVisible(record_status_);

    global_target_caption_.setText("Target LUFS:", juce::dontSendNotification);
    global_target_caption_.setFont(juce::FontOptions(12.0f));
    global_target_caption_.setJustificationType(juce::Justification::centredRight);
    global_target_caption_.setColour(juce::Label::textColourId,
                                      juce::Colours::white.withAlpha(0.75f));
    addAndMakeVisible(global_target_caption_);

    global_target_label_.setEditable(false, true, false);
    global_target_label_.setFont(juce::FontOptions(12.0f));
    global_target_label_.setJustificationType(juce::Justification::centred);
    global_target_label_.setColour(juce::Label::textColourId, juce::Colours::white);
    global_target_label_.setColour(juce::Label::backgroundColourId,
                                    juce::Colours::black.withAlpha(0.35f));
    global_target_label_.setColour(juce::Label::backgroundWhenEditingColourId,
                                    juce::Colours::black.withAlpha(0.6f));
    global_target_label_.setText(juce::String(processor_.getTargetLufs(), 1),
                                  juce::dontSendNotification);
    global_target_label_.setTooltip("Default LUFS target for new layers and 'Normalize All'.");
    global_target_label_.onTextChange = [this] {
        const auto new_v = global_target_label_.getText().getFloatValue();
        const auto old_v = processor_.getTargetLufs();
        if (std::abs(new_v - old_v) > 0.001f) {
            processor_.commandStack().execute(
                std::make_unique<gatherer::undo::SetGlobalTargetLufsCommand>(
                    processor_, new_v, old_v));
        }
        global_target_label_.setText(juce::String(processor_.getTargetLufs(), 1),
                                      juce::dontSendNotification);
    };
    addAndMakeVisible(global_target_label_);

    normalize_all_button_.setColour(juce::TextButton::buttonColourId,
                                     juce::Colours::black.withAlpha(0.4f));
    normalize_all_button_.setColour(juce::TextButton::textColourOffId,
                                     juce::Colours::white);
    normalize_all_button_.setTooltip("Set the per-layer normalize gain so every active "
                                     "layer hits the global target LUFS. Affects live "
                                     "preview only — use 'Export Normalized' to write to disk.");

    export_normalized_button_.setColour(juce::TextButton::buttonColourId,
                                         juce::Colours::black.withAlpha(0.4f));
    export_normalized_button_.setColour(juce::TextButton::textColourOffId,
                                         juce::Colours::white);
    export_normalized_button_.setTooltip("Render *_normalized.wav stems: each slot's audio "
                                          "with its current normalize gain, padded to the "
                                          "session timeline (offset + trailing silence) so the "
                                          "files are sample-aligned across all slots.");
    export_normalized_button_.onClick = [this] { processor_.exportNormalized(); };
    addAndMakeVisible(export_normalized_button_);

    export_aligned_button_.setColour(juce::TextButton::buttonColourId,
                                      juce::Colours::black.withAlpha(0.4f));
    export_aligned_button_.setColour(juce::TextButton::textColourOffId,
                                      juce::Colours::white);
    export_aligned_button_.setTooltip("Render *_aligned.wav stems: each slot's original audio "
                                       "(no gain applied), padded to the session timeline so the "
                                       "files are sample-aligned across all slots.");
    export_aligned_button_.onClick = [this] { processor_.exportAligned(); };
    addAndMakeVisible(export_aligned_button_);
    normalize_all_button_.onClick = [this] {
        // Normalize All uses the GLOBAL target (the field next to the button),
        // not per-row targets. Per-row targets remain in effect for the per-row
        // "N" button so layers can have their own overrides for individual
        // normalization.
        constexpr float kMin = -60.0f, kMax = 12.0f;
        const float global_target = processor_.getTargetLufs();
        std::vector<std::unique_ptr<gatherer::undo::Command>> kids;
        for (const auto& s : processor_.snapshotSatellites()) {
            const auto lufs = processor_.getSlotLufs(s.slot_index);
            if (lufs.integrated <= -99.0f) continue;
            const float new_db = juce::jlimit(kMin, kMax,
                                              global_target - lufs.integrated);
            const float old_db = processor_.getNormalizeDb(s.slot_index);
            if (std::abs(new_db - old_db) > 0.001f) {
                kids.push_back(std::make_unique<gatherer::undo::SetNormalizeDbCommand>(
                    processor_, s.slot_index, new_db, old_db));
            }
        }
        if (kids.empty()) return;
        processor_.commandStack().execute(
            std::make_unique<gatherer::undo::CompositeCommand>(
                "Normalize All", std::move(kids)));
    };
    addAndMakeVisible(normalize_all_button_);

    health_summary_.setText("health: collecting...", juce::dontSendNotification);
    health_summary_.setFont(juce::FontOptions(13.0f, juce::Font::bold));
    health_summary_.setJustificationType(juce::Justification::centredLeft);
    addAndMakeVisible(health_summary_);

    health_detail_.setText("", juce::dontSendNotification);
    health_detail_.setFont(juce::FontOptions(12.0f));
    health_detail_.setJustificationType(juce::Justification::topLeft);
    health_detail_.setColour(juce::Label::textColourId,
                             juce::Colours::white.withAlpha(0.7f));
    addAndMakeVisible(health_detail_);

    health_reset_button_.setTooltip("Clear the health monitor's history, re-evaluate, "
                                    "and clean up any orphaned slots whose plugin "
                                    "process is no longer running.");
    health_reset_button_.onClick = [this] {
        processor_.reclaimGhostSlots();
        health_monitor_.clear();
        health_summary_.setText("Collecting data...", juce::dontSendNotification);
        health_detail_.setText("", juce::dontSendNotification);
        health_badge_color_ = juce::Colours::lightgrey;
        repaint();
    };
    addAndMakeVisible(health_reset_button_);

    calibrate_summary_.setText("Calibration: not run", juce::dontSendNotification);
    calibrate_summary_.setFont(juce::FontOptions(13.0f, juce::Font::bold));
    calibrate_summary_.setJustificationType(juce::Justification::centredLeft);
    addAndMakeVisible(calibrate_summary_);

    calibrate_detail_.setText("Press Calibrate during playback to measure inter-satellite "
                              "alignment with sample accuracy.",
                              juce::dontSendNotification);
    calibrate_detail_.setFont(juce::FontOptions(12.0f));
    calibrate_detail_.setJustificationType(juce::Justification::topLeft);
    calibrate_detail_.setColour(juce::Label::textColourId,
                                juce::Colours::white.withAlpha(0.7f));
    addAndMakeVisible(calibrate_detail_);

    pdc_diag_label_.setText("calibrator: starting...", juce::dontSendNotification);
    pdc_diag_label_.setFont(juce::FontOptions(11.0f));
    pdc_diag_label_.setJustificationType(juce::Justification::topLeft);
    pdc_diag_label_.setColour(juce::Label::textColourId,
                              juce::Colours::yellow.withAlpha(0.85f));
    addAndMakeVisible(pdc_diag_label_);

    calibrate_button_.setTooltip("Run an active probe: hub posts a calibration session, "
                                 "each sat snapshots (hub_heartbeat, write_pos) when it "
                                 "sees the session, hub compares snapshots. Detects "
                                 "callback-level misalignment with sample accuracy.");
    calibrate_button_.onClick = [this] {
        processor_.startCalibration();
        calibrate_summary_.setText("Calibrating...", juce::dontSendNotification);
        calibrate_detail_.setText("Recording satellite responses...", juce::dontSendNotification);
        calibrate_badge_color_ = juce::Colours::lightgrey;
        repaint();
    };
    addAndMakeVisible(calibrate_button_);

    auto setupHistoryButton = [this](juce::TextButton& b, const juce::String& tooltip) {
        b.setColour(juce::TextButton::buttonColourId,
                    juce::Colours::black.withAlpha(0.4f));
        b.setColour(juce::TextButton::textColourOffId, juce::Colours::white);
        b.setTooltip(tooltip);
        addAndMakeVisible(b);
    };
    setupHistoryButton(undo_button_, "Undo (⌘Z)");
    setupHistoryButton(redo_button_, "Redo (⇧⌘Z)");
    undo_button_.onClick = [this] { processor_.commandStack().undo(); };
    redo_button_.onClick = [this] { processor_.commandStack().redo(); };
    processor_.commandStack().onChange = [this] { refreshUndoRedoButtons(); };
    refreshUndoRedoButtons();

    play_button_.setColour(juce::TextButton::buttonColourId,
                            juce::Colours::black.withAlpha(0.4f));
    play_button_.setColour(juce::TextButton::textColourOffId, juce::Colours::white);
    play_button_.setTooltip("Play / pause the recorded layers through the hub.");
    play_button_.onClick = [this] {
        auto& pb = processor_.playback();
        if (pb.isPlaying()) pb.pause();
        else                pb.play();
    };
    addAndMakeVisible(play_button_);

    stop_button_.setColour(juce::TextButton::buttonColourId,
                            juce::Colours::black.withAlpha(0.4f));
    stop_button_.setColour(juce::TextButton::textColourOffId, juce::Colours::white);
    stop_button_.setTooltip("Stop playback and return to the start.");
    stop_button_.onClick = [this] { processor_.playback().stop(); };
    addAndMakeVisible(stop_button_);

    transport_pos_.setText("0:00 / 0:00", juce::dontSendNotification);
    transport_pos_.setFont(juce::FontOptions(11.0f));
    transport_pos_.setJustificationType(juce::Justification::centredLeft);
    transport_pos_.setColour(juce::Label::textColourId,
                              juce::Colours::white.withAlpha(0.7f));
    addAndMakeVisible(transport_pos_);

    session_caption_.setText("Session:", juce::dontSendNotification);
    session_caption_.setFont(juce::FontOptions(12.0f));
    session_caption_.setJustificationType(juce::Justification::centredRight);
    session_caption_.setColour(juce::Label::textColourId,
                                juce::Colours::white.withAlpha(0.75f));
    addAndMakeVisible(session_caption_);

    session_name_.setFont(juce::FontOptions(12.0f, juce::Font::bold));
    session_name_.setJustificationType(juce::Justification::centredLeft);
    session_name_.setColour(juce::Label::textColourId, juce::Colours::white);
    session_name_.setColour(juce::Label::backgroundColourId,
                             juce::Colours::black.withAlpha(0.25f));
    addAndMakeVisible(session_name_);

    auto setupSessionBtn = [this](juce::TextButton& b, const juce::String& tip) {
        b.setColour(juce::TextButton::buttonColourId,
                    juce::Colours::black.withAlpha(0.4f));
        b.setColour(juce::TextButton::textColourOffId, juce::Colours::white);
        b.setTooltip(tip);
        addAndMakeVisible(b);
    };
    setupSessionBtn(session_new_button_,  "Start a new session (fresh recordings, current mix kept).");
    setupSessionBtn(session_open_button_, "Open an existing session folder.");
    setupSessionBtn(session_save_button_, "Save the current session manifest.");
    session_new_button_.onClick  = [this] { processor_.session().newSession(); };
    session_save_button_.onClick = [this] { processor_.session().save(); };
    session_open_button_.onClick = [this] {
        const auto start = processor_.session().hasSession()
            ? processor_.session().currentFolder().getParentDirectory()
            : gatherer::session::SessionManager::defaultParentFolder();
        session_chooser_ = std::make_unique<juce::FileChooser>(
            "Open a Gatherer session folder",
            start,
            juce::String{},
            true);
        const auto flags = juce::FileBrowserComponent::openMode
                          | juce::FileBrowserComponent::canSelectDirectories;
        session_chooser_->launchAsync(flags, [this](const juce::FileChooser& c) {
            const auto picked = c.getResult();
            if (picked.isDirectory()) processor_.session().openSession(picked);
        });
    };

    processor_.session().onChange = [this] {
        const auto name = processor_.session().currentName();
        session_name_.setText(name.isEmpty() ? "(none)" : name,
                              juce::dontSendNotification);
        session_save_button_.setEnabled(processor_.session().hasSession());
    };
    processor_.session().onChange();  // initial sync

    addAndMakeVisible(layers_container_);

    setSize(1200, 620);
    setWantsKeyboardFocus(true);
    // 30Hz for smooth meter ballistics. Health monitor + calibration polling
    // are time-based so they tolerate the faster tick fine.
    startTimerHz(30);
}

HubEditor::~HubEditor() {
    processor_.commandStack().onChange = nullptr;
    processor_.session()     .onChange = nullptr;
}

bool HubEditor::keyPressed(const juce::KeyPress& k) {
    using namespace juce;
    const auto mods = k.getModifiers();
    if (k.getKeyCode() == 'Z' && mods.isCommandDown()) {
        if (mods.isShiftDown()) processor_.commandStack().redo();
        else                    processor_.commandStack().undo();
        return true;
    }
    return false;
}

void HubEditor::refreshUndoRedoButtons() {
    auto& s = processor_.commandStack();
    undo_button_.setEnabled(s.canUndo());
    redo_button_.setEnabled(s.canRedo());
    const auto undoLbl = s.topUndoLabel();
    const auto redoLbl = s.topRedoLabel();
    undo_button_.setTooltip(undoLbl.isEmpty() ? "Nothing to undo (⌘Z)"
                                              : "Undo " + undoLbl + " (⌘Z)");
    redo_button_.setTooltip(redoLbl.isEmpty() ? "Nothing to redo (⇧⌘Z)"
                                              : "Redo " + redoLbl + " (⇧⌘Z)");
}

void HubEditor::paint(juce::Graphics& g) {
    g.fillAll(juce::Colours::darkslateblue.darker(0.4f));

    auto drawBadge = [&](const juce::Rectangle<int>& near, juce::Colour c) {
        const auto cy = near.getY() + near.getHeight() / 2;
        const auto cx = near.getX() - kBadgeSize / 2 - 6;
        g.setColour(c);
        g.fillEllipse(static_cast<float>(cx - kBadgeSize / 2),
                      static_cast<float>(cy - kBadgeSize / 2),
                      static_cast<float>(kBadgeSize),
                      static_cast<float>(kBadgeSize));
        g.setColour(juce::Colours::black.withAlpha(0.4f));
        g.drawEllipse(static_cast<float>(cx - kBadgeSize / 2),
                      static_cast<float>(cy - kBadgeSize / 2),
                      static_cast<float>(kBadgeSize),
                      static_cast<float>(kBadgeSize), 1.0f);
    };

    drawBadge(health_summary_.getBounds(),    health_badge_color_);
    drawBadge(calibrate_summary_.getBounds(), calibrate_badge_color_);
}

void HubEditor::resized() {
    auto area = getLocalBounds().reduced(12);
    title_.setBounds(area.removeFromTop(32));
    area.removeFromTop(4);
    status_.setBounds(area.removeFromTop(22));
    area.removeFromTop(2);
    {
        auto opt_row = area.removeFromTop(22);
        master_record_button_.setBounds(opt_row.removeFromRight(100).reduced(0, 1));
        opt_row.removeFromRight(8);
        record_status_.setBounds(opt_row.removeFromRight(220));
        opt_row.removeFromRight(12);
        export_aligned_button_.setBounds(opt_row.removeFromRight(120).reduced(0, 1));
        opt_row.removeFromRight(6);
        export_normalized_button_.setBounds(opt_row.removeFromRight(140).reduced(0, 1));
        opt_row.removeFromRight(6);
        normalize_all_button_.setBounds(opt_row.removeFromRight(110).reduced(0, 1));
        opt_row.removeFromRight(6);
        global_target_label_.setBounds(opt_row.removeFromRight(50));
        opt_row.removeFromRight(4);
        global_target_caption_.setBounds(opt_row.removeFromRight(90));
        opt_row.removeFromRight(12);
        undo_button_.setBounds(opt_row.removeFromLeft(56).reduced(0, 1));
        opt_row.removeFromLeft(4);
        redo_button_.setBounds(opt_row.removeFromLeft(56).reduced(0, 1));
        opt_row.removeFromLeft(12);
        // Split the leftover space between the two toggles.
        const int toggle_w = opt_row.getWidth() / 2;
        include_input_toggle_.setBounds(opt_row.removeFromLeft(toggle_w));
        pad_silence_toggle_  .setBounds(opt_row);
    }
    area.removeFromTop(6);

    // Health row.
    {
        auto health_area = area.removeFromTop(70);
        health_area.removeFromLeft(kBadgeSize + 12);
        auto summary_row = health_area.removeFromTop(22);
        health_reset_button_.setBounds(summary_row.removeFromRight(90).reduced(0, 2));
        summary_row.removeFromRight(8);
        health_summary_.setBounds(summary_row);
        health_detail_.setBounds(health_area);
    }

    area.removeFromTop(10);

    // Calibration row — taller because the audio-correlation verdict can run long.
    {
        auto cal_area = area.removeFromTop(110);
        cal_area.removeFromLeft(kBadgeSize + 12);
        auto summary_row = cal_area.removeFromTop(22);
        calibrate_button_.setBounds(summary_row.removeFromRight(90).reduced(0, 2));
        summary_row.removeFromRight(8);
        calibrate_summary_.setBounds(summary_row);
        calibrate_detail_.setBounds(cal_area);
    }

    area.removeFromTop(4);
    pdc_diag_label_.setBounds(area.removeFromTop(18));
    area.removeFromTop(8);

    // Session row.
    {
        auto s_row = area.removeFromTop(24);
        session_caption_.setBounds(s_row.removeFromLeft(70));
        s_row.removeFromLeft(4);
        session_name_   .setBounds(s_row.removeFromLeft(280).reduced(0, 1));
        s_row.removeFromLeft(10);
        session_new_button_ .setBounds(s_row.removeFromLeft(56).reduced(0, 1));
        s_row.removeFromLeft(4);
        session_open_button_.setBounds(s_row.removeFromLeft(56).reduced(0, 1));
        s_row.removeFromLeft(4);
        session_save_button_.setBounds(s_row.removeFromLeft(56).reduced(0, 1));
    }
    area.removeFromTop(4);

    // Transport row, just above the layers.
    {
        auto t_row = area.removeFromTop(24);
        play_button_  .setBounds(t_row.removeFromLeft(60).reduced(0, 1));
        t_row.removeFromLeft(4);
        stop_button_  .setBounds(t_row.removeFromLeft(60).reduced(0, 1));
        t_row.removeFromLeft(10);
        transport_pos_.setBounds(t_row.removeFromLeft(160));
    }
    area.removeFromTop(4);

    layers_container_.setBounds(area);
}

namespace {
juce::String formatMmSs(double seconds) {
    if (seconds < 0) seconds = 0;
    const int total = static_cast<int>(seconds);
    return juce::String::formatted("%d:%02d", total / 60, total % 60);
}
}  // namespace

void HubEditor::timerCallback() {
    // Transport state. Play/Stop reflect playback engine state; the time
    // display uses the playback engine's session length (steady value updated
    // on recomputeSessionLayout) — during recording the per-lane width may
    // grow with the live thumbnail, but the transport clock stays anchored to
    // the playback engine for predictability.
    {
        auto& pb = processor_.playback();
        play_button_.setButtonText(pb.isPlaying() ? "Pause" : "Play");
        const bool has_session = pb.sessionLengthSamples() > 0;
        play_button_.setEnabled(has_session);
        stop_button_.setEnabled(has_session);
        transport_pos_.setText(formatMmSs(pb.playheadSeconds()) + " / "
                                 + formatMmSs(pb.sessionLengthSeconds()),
                                juce::dontSendNotification);
    }

    const int n = processor_.activeSatellites();
    status_.setText((processor_.isHub() ? juce::String("hub: ACTIVE  ")
                                        : juce::String("hub: NOT REGISTERED  "))
                    + "satellites: " + juce::String(n),
                    juce::dontSendNotification);

    updateHealth();

    // PDC calibrator diagnostic.
    {
        const auto ticks    = processor_.pdcTickCount();
        const auto succ     = processor_.pdcSuccessCount();
        juce::String txt = "calibrator: ticks=" + juce::String((long long) ticks)
                          + " measurements=" + juce::String((long long) succ);
        // Show per-slot skip reason or measured value so we can see WHY
        // the calibrator isn't publishing.
        auto skipName = [](HubProcessor::PdcSkip s) -> const char* {
            using S = HubProcessor::PdcSkip;
            switch (s) {
                case S::Ok:                  return "ok";
                case S::SlotInactive:        return "inactive";
                case S::SatNotWritten:       return "sat-no-writes";
                case S::SatNotEnoughData:    return "sat-need-more";
                case S::SatRingOverrun:      return "sat-overrun";
                case S::SatSilent:           return "sat-silent";
                case S::HubWindowBeforeZero: return "hub<0";
                case S::HubWindowPastWrite:  return "hub>wp";
                case S::HubWindowOutOfCap:   return "hub-too-old";
                case S::HubSilent:           return "hub-silent";
            }
            return "?";
        };
        const double sr = processor_.getSampleRate();
        for (std::uint32_t i = 0; i < gatherer::protocol::NUM_SLOTS; ++i) {
            if (processor_.pdcLastSkip(static_cast<int>(i)) == HubProcessor::PdcSkip::SlotInactive
                && processor_.pdcDMeasured(static_cast<int>(i)) == HubProcessor::kPdcUnknown) continue;
            txt += "  s" + juce::String((int) i) + ":";
            const auto d = processor_.pdcDMeasured(static_cast<int>(i));
            if (d != HubProcessor::kPdcUnknown) {
                const double ms   = (sr > 0.0) ? (static_cast<double>(d) / sr) * 1000.0 : 0.0;
                const float  conf = processor_.pdcConfidence(static_cast<int>(i));
                txt += juce::String((long long) d) + "s("
                     + juce::String(ms, 1) + "ms@"
                     + juce::String(conf, 2) + ")";
            } else {
                txt += juce::String(skipName(processor_.pdcLastSkip(static_cast<int>(i))));
            }
        }
        pdc_diag_label_.setText(txt, juce::dontSendNotification);
    }

    // Master record button state + status text.
    if (processor_.isArmedPending()) {
        master_record_button_.setButtonText("◌ Armed");
        master_record_button_.setColour(juce::TextButton::buttonColourId,
                                         juce::Colours::gold.withAlpha(0.6f));
        master_record_button_.setColour(juce::TextButton::textColourOffId,
                                         juce::Colours::black);
        record_status_.setText("Waiting for DAW transport to play...",
                                juce::dontSendNotification);
    } else if (processor_.isRecording()) {
        master_record_button_.setButtonText("■ Stop");
        master_record_button_.setColour(juce::TextButton::buttonColourId,
                                         juce::Colours::indianred);
        master_record_button_.setColour(juce::TextButton::textColourOffId,
                                         juce::Colours::white);
        const auto folder = processor_.currentRecordingFolder();
        if (folder.exists()) {
            record_status_.setText("Recording → " + folder.getFileName(),
                                    juce::dontSendNotification);
        }
    } else {
        master_record_button_.setButtonText("● Record");
        master_record_button_.setColour(juce::TextButton::buttonColourId,
                                         juce::Colours::black.withAlpha(0.4f));

        // Detect the rising edge of "normalization just finished" → auto-save
        // the session so the manifest records the new *_normalized.wav files.
        {
            const bool now_norm = processor_.isNormalizing();
            if (was_normalizing_ && !now_norm) {
                processor_.session().autoSave();
            }
            was_normalizing_ = now_norm;
        }

        if (processor_.isNormalizing()) {
            const auto results = processor_.lastNormalizationResults();
            record_status_.setText("Exporting... ("
                                    + juce::String(static_cast<int>(results.size()))
                                    + " done)",
                                    juce::dontSendNotification);
        } else {
            const auto results = processor_.lastNormalizationResults();
            if (!results.empty()) {
                int succeeded = 0;
                for (const auto& r : results) if (r.success) ++succeeded;
                record_status_.setText("Exported "
                                        + juce::String(succeeded) + "/"
                                        + juce::String(static_cast<int>(results.size()))
                                        + " normalized file(s)",
                                        juce::dontSendNotification);
            } else {
                record_status_.setText("", juce::dontSendNotification);
            }
        }
    }

    // Drive the calibration probe.
    processor_.finishCalibrationIfReady();
    const auto cal = processor_.lastCalibrationResult();
    if (processor_.calibrationInProgress()) {
        calibrate_badge_color_ = juce::Colours::lightgrey;
        calibrate_button_.setEnabled(false);
    } else {
        calibrate_button_.setEnabled(true);
        if (cal.valid) {
            calibrate_badge_color_ = cal.passed ? juce::Colours::limegreen
                                                : juce::Colours::indianred;
            calibrate_summary_.setText(juce::String(cal.summary), juce::dontSendNotification);
            calibrate_detail_.setText(juce::String(cal.detail), juce::dontSendNotification);
        }
    }

    // Build the set of active slots; create rows on demand. Rows persist across
    // activity changes so they don't flicker.
    std::array<bool, gatherer::protocol::NUM_SLOTS> active_now{};
    const auto sats = processor_.snapshotSatellites();

    // Compute the lane time-axis once per tick. While recording, the thumbnail
    // for an in-flight slot is still growing — fold its live length into the
    // session bound so lane widths and other slots' relative offsets reflect
    // current state rather than only what the playback engine knows.
    //
    // Skip slots with no audio (no thumbnail length AND no playback source) —
    // they're either ghost slots or slots that haven't been recorded yet, and
    // their grid info shouldn't be allowed to bound the lane time axis.
    double session_seconds_for_lanes = processor_.playback().sessionLengthSeconds();
    {
        const auto session_grid = processor_.getCurrentGridInfo();
        const double sr = std::max(1.0, processor_.getSampleRate());
        if (session_grid.captured) {
            const double session_start_b = processor_.getSessionStartInBeats();
            for (int i = 0; i < static_cast<int>(gatherer::protocol::NUM_SLOTS); ++i) {
                const auto sg = processor_.getSlotGridInfo(i);
                double length_sec = 0.0;
                if (auto* tn = processor_.getThumbnail(i); tn != nullptr && tn->getTotalLength() > 0.0) {
                    length_sec = tn->getTotalLength();
                } else {
                    length_sec = processor_.playback().slotLengthSamples(i) / sr;
                }
                if (length_sec <= 0.0) continue;  // ghost / unused slot
                double offset_sec = 0.0;
                if (sg.captured) {
                    offset_sec = (sg.start_in_beats - session_start_b) * 60.0 / session_grid.bpm;
                    if (offset_sec < 0.0) offset_sec = 0.0;
                }
                session_seconds_for_lanes = std::max(session_seconds_for_lanes,
                                                      offset_sec + length_sec);
            }
        }
    }
    for (const auto& s : sats) {
        if (s.slot_index < 0 || s.slot_index >= static_cast<int>(rows_.size())) continue;

        auto& row_ptr = rows_[static_cast<std::size_t>(s.slot_index)];
        if (!row_ptr) {
            row_ptr = std::make_unique<LayerRow>();
            const int slot_idx = s.slot_index;
            row_ptr->onMuteChanged = [this, slot_idx](bool on) {
                const bool old_v = processor_.getMute(slot_idx);
                if (on == old_v) return;
                processor_.commandStack().execute(
                    std::make_unique<gatherer::undo::SetMuteCommand>(
                        processor_, slot_idx, on, old_v));
            };
            row_ptr->onSoloChanged = [this, slot_idx](bool on) {
                const bool old_v = processor_.getSolo(slot_idx);
                if (on == old_v) return;
                processor_.commandStack().execute(
                    std::make_unique<gatherer::undo::SetSoloCommand>(
                        processor_, slot_idx, on, old_v));
            };
            row_ptr->onGainDbChanged = [this, slot_idx](float db) {
                const float old_db = processor_.getGainDb(slot_idx);
                if (std::abs(db - old_db) < 0.001f) return;
                processor_.commandStack().execute(
                    std::make_unique<gatherer::undo::SetGainDbCommand>(
                        processor_, slot_idx, db, old_db));
            };
            row_ptr->onRecordArmChanged = [this, slot_idx](bool on) {
                const bool old_v = processor_.getRecordArm(slot_idx);
                if (on == old_v) return;
                processor_.commandStack().execute(
                    std::make_unique<gatherer::undo::SetRecordArmCommand>(
                        processor_, slot_idx, on, old_v));
            };
            row_ptr->onNormalize = [this, slot_idx] {
                const auto lufs = processor_.getSlotLufs(slot_idx);
                if (lufs.integrated <= -99.0f) return;
                const float target = processor_.getSlotTargetLufs(slot_idx);
                const float new_db = juce::jlimit(-60.0f, 12.0f,
                                                  target - lufs.integrated);
                const float old_db = processor_.getNormalizeDb(slot_idx);
                if (std::abs(new_db - old_db) < 0.001f) return;
                processor_.commandStack().execute(
                    std::make_unique<gatherer::undo::SetNormalizeDbCommand>(
                        processor_, slot_idx, new_db, old_db));
            };
            row_ptr->onTargetLufsChanged = [this, slot_idx](float v) {
                const float old_v = processor_.getSlotTargetLufs(slot_idx);
                if (std::abs(v - old_v) < 0.001f) return;
                processor_.commandStack().execute(
                    std::make_unique<gatherer::undo::SetSlotTargetLufsCommand>(
                        processor_, slot_idx, v, old_v));
            };
            row_ptr->onSeekSeconds = [this](double s) {
                processor_.playback().seekSeconds(s);
            };
            row_ptr->onDeleteRecording = [this, slot_idx] {
                const auto wav = processor_.getLastRecordingForSlot(slot_idx);
                if (wav == juce::File{} || !wav.existsAsFile()) return;
                processor_.commandStack().execute(
                    std::make_unique<gatherer::undo::DeleteRecordingCommand>(
                        processor_, slot_idx, wav));
            };
            row_ptr->onMoveUp   = [this, slot_idx] {
                processor_.moveSlotInDisplayOrder(slot_idx, -1);
            };
            row_ptr->onMoveDown = [this, slot_idx] {
                processor_.moveSlotInDisplayOrder(slot_idx, +1);
            };
            row_ptr->onPdcOverrideChanged = [this, slot_idx](long long samples) {
                if (samples == LayerRow::kPdcUnknown) {
                    processor_.clearPdcDOverride(slot_idx);
                } else {
                    processor_.setPdcDOverride(slot_idx, samples);
                }
            };
            // New row → inherit the current global target if the per-slot target
            // is still at its default. (State-restore paths preserve whatever
            // was saved.)
            if (std::abs(processor_.getSlotTargetLufs(slot_idx) - (-14.0f)) < 0.001f) {
                processor_.setSlotTargetLufs(slot_idx, processor_.getTargetLufs());
            }
            row_ptr->setThumbnail(processor_.getThumbnail(slot_idx));
            layers_container_.addAndMakeVisible(*row_ptr);
        }
        row_ptr->setIdentity(s.display_name, s.track_name, s.color_rgba);
        row_ptr->setMixState(processor_.getMute(s.slot_index),
                              processor_.getSolo(s.slot_index),
                              processor_.getRecordArm(s.slot_index),
                              processor_.getGainDb(s.slot_index),
                              processor_.getNormalizeDb(s.slot_index),
                              processor_.getSlotTargetLufs(s.slot_index));

        const auto lvl = processor_.getSlotLevels(s.slot_index);
        row_ptr->setLevels(lvl.peak_db_l, lvl.rms_db_l, lvl.peak_db_r, lvl.rms_db_r);

        const auto lufs = processor_.getSlotLufs(s.slot_index);
        row_ptr->setLufs(lufs.integrated, lufs.momentary, lufs.short_term);

        // Per-sat PDC state (auto-measured + user override).
        {
            const auto m  = processor_.pdcDMeasured(s.slot_index);
            const auto ov = processor_.pdcDOverride(s.slot_index);
            row_ptr->setPdcState(
                m  == HubProcessor::kPdcUnknown ? LayerRow::kPdcUnknown : static_cast<long long>(m),
                ov == HubProcessor::kPdcUnknown ? LayerRow::kPdcUnknown : static_cast<long long>(ov),
                processor_.getSampleRate());
        }

        const auto rec_file = processor_.getLastRecordingForSlot(s.slot_index);
        row_ptr->setRecordingAvailable(rec_file != juce::File{} && rec_file.existsAsFile());
        row_ptr->setPlayheadSeconds(processor_.playback().playheadSeconds());

        // Live session layout — compute slot offsets and session length here
        // (rather than reading from PlaybackEngine, which only updates after
        // stopRecording). This way the visual offset of an in-flight
        // re-recording snaps to the right position as soon as its per-slot
        // grid info is captured on the first audio block.
        const auto g  = processor_.getCurrentGridInfo();
        const auto sg = processor_.getSlotGridInfo(s.slot_index);
        const double sr = std::max(1.0, processor_.getSampleRate());

        double slot_offset_sec = 0.0;
        if (g.captured && sg.captured) {
            slot_offset_sec = (sg.start_in_beats - processor_.getSessionStartInBeats())
                            * 60.0 / g.bpm;
            if (slot_offset_sec < 0.0) slot_offset_sec = 0.0;
        }
        double slot_length_sec = 0.0;
        if (auto* tn = processor_.getThumbnail(s.slot_index); tn != nullptr && tn->getTotalLength() > 0.0) {
            slot_length_sec = tn->getTotalLength();
        } else {
            slot_length_sec = processor_.playback().slotLengthSamples(s.slot_index) / sr;
        }

        if (g.captured) {
            row_ptr->setGridBpm(g.bpm);
            row_ptr->setGridTimeSigNum(g.time_sig_num);
            row_ptr->setGridStartInBeats(processor_.getSessionStartInBeats());
        } else {
            row_ptr->setGridBpm(0.0);  // disables grid drawing
        }
        row_ptr->setSessionLengthSeconds(session_seconds_for_lanes);
        row_ptr->setSlotLayout(slot_offset_sec, slot_length_sec);

        active_now[static_cast<std::size_t>(s.slot_index)] = true;
    }

    std::size_t count_now = 0;
    for (std::size_t i = 0; i < rows_.size(); ++i) {
        if (rows_[i]) rows_[i]->setVisible(active_now[i]);
        if (active_now[i]) ++count_now;
    }

    // Walk display_order, collect visible slots in display order. Re-layout
    // whenever count *or* order changes — both are user-driven (sat
    // connect/disconnect, or move up/down click).
    const auto order = processor_.getDisplayOrder();
    std::array<int, gatherer::protocol::NUM_SLOTS> visible_seq{};
    std::size_t visible_count = 0;
    for (int slot : order) {
        if (slot >= 0 && slot < static_cast<int>(active_now.size())
            && active_now[static_cast<std::size_t>(slot)]) {
            visible_seq[visible_count++] = slot;
        }
    }

    const bool layout_changed =
        visible_count != last_layout_count_
        || !std::equal(visible_seq.begin(), visible_seq.begin() + visible_count,
                        last_layout_order_.begin());
    if (layout_changed) {
        last_layout_count_ = visible_count;
        std::copy(visible_seq.begin(), visible_seq.begin() + visible_count,
                   last_layout_order_.begin());

        constexpr int kRowHeight = 56;
        constexpr int kRowGap    = 4;

        const auto bounds = layers_container_.getLocalBounds();
        int y = bounds.getY();
        for (std::size_t i = 0; i < visible_count; ++i) {
            const int slot = visible_seq[i];
            auto& r = rows_[static_cast<std::size_t>(slot)];
            if (!r) continue;
            r->setBounds(bounds.getX(), y, bounds.getWidth(), kRowHeight);
            r->setMoveUpEnabled  (i > 0);
            r->setMoveDownEnabled(i + 1 < visible_count);
            y += kRowHeight + kRowGap;
        }
    }
}

void HubEditor::updateHealth() {
    using namespace gatherer::diagnostics;

    Sample sample;
    sample.at             = std::chrono::steady_clock::now();
    sample.hub_heartbeat  = processor_.hubHeartbeat();
    sample.max_block_size = processor_.maxBlockSize();

    for (const auto& s : processor_.snapshotSatellites()) {
        if (s.slot_index < 0 || s.slot_index >= static_cast<int>(sample.slots.size())) continue;
        auto& slot                  = sample.slots[s.slot_index];
        slot.active                 = true;
        slot.uuid                   = s.uuid;
        slot.heartbeat              = s.heartbeat;
        slot.write_pos              = s.write_pos;
        slot.last_write_host_frame  = s.last_write_host_frame;
    }

    health_monitor_.tick(sample);
    const auto h = health_monitor_.current();

    switch (h.level) {
        case HealthLevel::Green:   health_badge_color_ = juce::Colours::limegreen;     break;
        case HealthLevel::Yellow:  health_badge_color_ = juce::Colours::gold;          break;
        case HealthLevel::Red:     health_badge_color_ = juce::Colours::indianred;     break;
        case HealthLevel::Unknown: health_badge_color_ = juce::Colours::lightgrey;     break;
    }

    health_summary_.setText(juce::String(h.summary), juce::dontSendNotification);
    health_detail_.setText(juce::String(h.detail),   juce::dontSendNotification);
    repaint();  // for the badge color
}

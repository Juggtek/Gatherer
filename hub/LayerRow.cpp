#include "LayerRow.h"

namespace {
// Strip is laid out left-to-right in signal-flow order:
//   [move ↑/↓][color][info][R][NORM: target + N + dB][VOL][M][S][meters][readout]
//
// The move column is a small reorder handle (up + down stacked) so the user
// can permute layers in display order. R is the record-arm tap (pre-fader).
// NORM is the LUFS-normalization gain stage, separate from the user fader so
// the upcoming Adaptive Mixer can drive the fader without fighting
// normalization. M/S are gates applied after both gain stages.
constexpr int kMoveColumnWidth = 20;
constexpr int kColorTagWidth   = 5;
constexpr int kInfoAreaWidth   = 130;
constexpr int kButtonSize      = 22;
constexpr int kSectionGap      = 6;

constexpr int kNormTargetW     = 40;     // editable per-slot target LUFS
constexpr int kNormInnerGap    = 3;
constexpr int kNormDbW         = 40;     // readout of applied normalize gain
constexpr int kNormAreaWidth   = kNormTargetW + kNormInnerGap + kButtonSize
                                + kNormInnerGap + kNormDbW;        // 105

constexpr int kFaderWidth      = 80;
constexpr int kMeterWidth      = 70;
constexpr int kMeterMarginX    = 6;
constexpr int kMeterBarHeight  = 7;
constexpr int kMeterBarGap     = 2;
constexpr int kReadoutWidth    = 80;
constexpr int kStripPadRight   = 8;

constexpr int kStripFixedWidth = kMoveColumnWidth
                                + kColorTagWidth + kInfoAreaWidth + kSectionGap
                                + kButtonSize + kSectionGap
                                + kNormAreaWidth + kSectionGap
                                + kFaderWidth + kSectionGap
                                + kButtonSize + kSectionGap
                                + kButtonSize + kSectionGap
                                + kMeterWidth + kMeterMarginX
                                + kReadoutWidth + kStripPadRight;

constexpr float kGainDbMin = -60.0f;
constexpr float kGainDbMax =  12.0f;
}

LayerRow::LayerRow() {
    track_.setFont(juce::FontOptions(14.0f, juce::Font::bold));
    track_.setJustificationType(juce::Justification::centredLeft);
    track_.setColour(juce::Label::textColourId, juce::Colours::white);
    addAndMakeVisible(track_);

    name_.setFont(juce::FontOptions(11.0f));
    name_.setJustificationType(juce::Justification::centredLeft);
    name_.setColour(juce::Label::textColourId,
                    juce::Colours::white.withAlpha(0.55f));
    addAndMakeVisible(name_);

    auto setupToggle = [this](juce::TextButton& b, juce::Colour onColour) {
        b.setClickingTogglesState(true);
        b.setColour(juce::TextButton::buttonColourId,
                    juce::Colours::black.withAlpha(0.35f));
        b.setColour(juce::TextButton::buttonOnColourId, onColour);
        b.setColour(juce::TextButton::textColourOffId,
                    juce::Colours::white.withAlpha(0.75f));
        b.setColour(juce::TextButton::textColourOnId, juce::Colours::black);
        addAndMakeVisible(b);
    };
    setupToggle(mute_button_,   juce::Colours::orange);
    setupToggle(solo_button_,   juce::Colours::gold);
    setupToggle(record_button_, juce::Colours::red);

    mute_button_.setTooltip  ("Mute this layer.");
    solo_button_.setTooltip  ("Solo this layer (when any solo is on, only soloed layers play).");
    record_button_.setTooltip("Record-arm this layer.");

    mute_button_.onClick   = [this] {
        if (onMuteChanged) onMuteChanged(mute_button_.getToggleState());
    };
    solo_button_.onClick   = [this] {
        if (onSoloChanged) onSoloChanged(solo_button_.getToggleState());
    };
    record_button_.onClick = [this] {
        if (onRecordArmChanged) onRecordArmChanged(record_button_.getToggleState());
    };

    // NORM section — momentary "N" button + editable target + readout of the
    // applied normalize-stage gain. The button writes the *normalize* gain on
    // the processor, never the user-facing fader.
    normalize_button_.setColour(juce::TextButton::buttonColourId,
                                juce::Colours::black.withAlpha(0.35f));
    normalize_button_.setColour(juce::TextButton::textColourOffId,
                                juce::Colours::white.withAlpha(0.75f));
    normalize_button_.setTooltip("Normalize this layer to its target LUFS. "
                                  "Writes the normalize-gain stage only — the volume "
                                  "fader is independent.");
    normalize_button_.onClick = [this] {
        if (onNormalize) onNormalize();
    };
    addAndMakeVisible(normalize_button_);

    norm_target_label_.setEditable(false, true, false);
    norm_target_label_.setFont(juce::FontOptions(11.0f));
    norm_target_label_.setJustificationType(juce::Justification::centred);
    norm_target_label_.setColour(juce::Label::textColourId, juce::Colours::white);
    norm_target_label_.setColour(juce::Label::backgroundColourId,
                                  juce::Colours::black.withAlpha(0.35f));
    norm_target_label_.setColour(juce::Label::backgroundWhenEditingColourId,
                                  juce::Colours::black.withAlpha(0.6f));
    norm_target_label_.setColour(juce::Label::textWhenEditingColourId, juce::Colours::white);
    norm_target_label_.setText("-14.0", juce::dontSendNotification);
    norm_target_label_.setTooltip("Per-layer target LUFS. Click to edit.");
    norm_target_label_.onTextChange = [this] {
        const auto v = norm_target_label_.getText().getFloatValue();
        if (onTargetLufsChanged) onTargetLufsChanged(v);
        norm_target_label_.setText(juce::String(v, 1), juce::dontSendNotification);
    };
    addAndMakeVisible(norm_target_label_);

    norm_db_label_.setFont(juce::FontOptions(10.0f));
    norm_db_label_.setJustificationType(juce::Justification::centred);
    norm_db_label_.setColour(juce::Label::textColourId,
                              juce::Colours::white.withAlpha(0.6f));
    norm_db_label_.setText("0.0 dB", juce::dontSendNotification);
    norm_db_label_.setTooltip("Current normalize-stage gain.");
    addAndMakeVisible(norm_db_label_);

    gain_slider_.setSliderStyle(juce::Slider::LinearHorizontal);
    gain_slider_.setRange(kGainDbMin, kGainDbMax, 0.1);
    gain_slider_.setValue(0.0, juce::dontSendNotification);
    gain_slider_.setTextBoxStyle(juce::Slider::NoTextBox, false, 0, 0);
    gain_slider_.setColour(juce::Slider::trackColourId,
                           juce::Colours::white.withAlpha(0.25f));
    gain_slider_.setColour(juce::Slider::thumbColourId, juce::Colours::white);
    gain_slider_.setDoubleClickReturnValue(true, 0.0);
    gain_slider_.setTooltip("Volume fader (will be driven by the Adaptive Mixer).");
    gain_slider_.onValueChange = [this] {
        if (onGainDbChanged) onGainDbChanged(static_cast<float>(gain_slider_.getValue()));
    };
    addAndMakeVisible(gain_slider_);

    move_up_button_  .setTooltip("Move this layer up in the display order.");
    move_down_button_.setTooltip("Move this layer down in the display order.");
    move_up_button_  .onClick = [this] { if (onMoveUp)   onMoveUp();   };
    move_down_button_.onClick = [this] { if (onMoveDown) onMoveDown(); };
    addAndMakeVisible(move_up_button_);
    addAndMakeVisible(move_down_button_);

    lane_.onDeleteRecording = [this] { if (onDeleteRecording) onDeleteRecording(); };
    lane_.onSeekFraction    = [this](double f) {
        if (onSeekSeconds && session_seconds_ > 0.0) {
            onSeekSeconds(f * session_seconds_);
        }
    };
    addAndMakeVisible(lane_);
}

void LayerRow::updatePlayhead() {
    const double f = (session_seconds_ > 0.0)
        ? juce::jlimit(0.0, 1.0, playhead_seconds_ / session_seconds_)
        : 0.0;
    lane_.setPlayheadFraction(f);
}

void LayerRow::updateGrid() {
    lane_.setGrid(grid_bpm_, grid_tsn_, grid_start_b_, session_seconds_);
}

void LayerRow::setIdentity(const juce::String& display_name,
                           const juce::String& track_name,
                           std::uint32_t       color_rgba) {
    name_ .setText(display_name, juce::dontSendNotification);
    track_.setText(track_name,   juce::dontSendNotification);

    if ((color_rgba & 0xFFu) == 0u) {
        color_ = juce::Colours::cornflowerblue;
    } else {
        const auto r = static_cast<juce::uint8>((color_rgba >> 24) & 0xFFu);
        const auto g = static_cast<juce::uint8>((color_rgba >> 16) & 0xFFu);
        const auto b = static_cast<juce::uint8>((color_rgba >>  8) & 0xFFu);
        color_ = juce::Colour::fromRGB(r, g, b);
    }
    repaint();
}

void LayerRow::setLevels(float peak_db_l, float rms_db_l,
                          float peak_db_r, float rms_db_r) {
    if (peak_db_l != peak_db_l_ || rms_db_l != rms_db_l_
        || peak_db_r != peak_db_r_ || rms_db_r != rms_db_r_) {
        peak_db_l_ = peak_db_l;
        rms_db_l_  = rms_db_l;
        peak_db_r_ = peak_db_r;
        rms_db_r_  = rms_db_r;
        repaint(meterAreaL().getUnion(meterAreaR()).getUnion(readoutArea()));
    }
}

void LayerRow::setLufs(float integrated, float momentary, float short_term) {
    if (integrated != lufs_integrated_ || momentary != lufs_momentary_
            || short_term != lufs_short_term_) {
        lufs_integrated_ = integrated;
        lufs_momentary_  = momentary;
        lufs_short_term_ = short_term;
        repaint(readoutArea());
    }
}

void LayerRow::setMixState(bool mute, bool solo, bool record_arm,
                           float gain_db, float norm_db, float target_lufs) {
    mute_button_  .setToggleState(mute,       juce::dontSendNotification);
    solo_button_  .setToggleState(solo,       juce::dontSendNotification);
    record_button_.setToggleState(record_arm, juce::dontSendNotification);
    gain_slider_  .setValue(gain_db,          juce::dontSendNotification);

    const juce::String norm_txt = (norm_db >= 0.0f ? "+" : "") + juce::String(norm_db, 1) + " dB";
    if (norm_db_label_.getText() != norm_txt) {
        norm_db_label_.setText(norm_txt, juce::dontSendNotification);
    }

    // Don't clobber the field while the user is mid-edit.
    if (!norm_target_label_.isBeingEdited()) {
        const juce::String t_txt = juce::String(target_lufs, 1);
        if (norm_target_label_.getText() != t_txt) {
            norm_target_label_.setText(t_txt, juce::dontSendNotification);
        }
    }
}

void LayerRow::paint(juce::Graphics& g) {
    auto strip = stripArea();
    g.setColour(juce::Colours::black.withAlpha(0.18f));
    g.fillRoundedRectangle(strip.toFloat(), 3.0f);

    g.setColour(color_);
    g.fillRect(strip.getX() + kMoveColumnWidth, strip.getY(),
               kColorTagWidth, strip.getHeight());

    auto drawBar = [&](const juce::Rectangle<int>& meter,
                       float peak_db, float rms_db) {
        g.setColour(juce::Colours::black.withAlpha(0.5f));
        g.fillRoundedRectangle(meter.toFloat(), 2.0f);

        const float rms_frac  = dbToFraction(rms_db);
        const float peak_frac = dbToFraction(peak_db);

        if (rms_frac > 0.0f) {
            const int w = juce::roundToInt(static_cast<float>(meter.getWidth()) * rms_frac);
            auto rms_rect = meter.withWidth(w).reduced(1);
            juce::ColourGradient grad(juce::Colours::limegreen,
                                       static_cast<float>(meter.getX()),
                                       static_cast<float>(meter.getY()),
                                       juce::Colours::red,
                                       static_cast<float>(meter.getRight()),
                                       static_cast<float>(meter.getY()),
                                       false);
            grad.addColour(0.7, juce::Colours::yellow);
            g.setGradientFill(grad);
            g.fillRect(rms_rect);
        }
        if (peak_frac > 0.0f) {
            const int x = meter.getX()
                        + juce::roundToInt(static_cast<float>(meter.getWidth()) * peak_frac);
            g.setColour(peak_db > 0.0f ? juce::Colours::red
                                        : juce::Colours::white.withAlpha(0.85f));
            g.fillRect(x - 1, meter.getY() + 1, 2, meter.getHeight() - 2);
        }
    };
    drawBar(meterAreaL(), peak_db_l_, rms_db_l_);
    drawBar(meterAreaR(), peak_db_r_, rms_db_r_);

    // Readout column: peak dB + 3-line LUFS (I/M/S) stacked.
    {
        auto ro = readoutArea();
        const int line_h = ro.getHeight() / 4;

        const float peak_max = std::max(peak_db_l_, peak_db_r_);
        g.setColour(juce::Colours::white.withAlpha(0.85f));
        g.setFont(juce::FontOptions(11.0f));
        const juce::String peak_txt = (peak_max <= -99.0f)
            ? juce::String("-inf")
            : juce::String(peak_max, 1) + " dB";
        g.drawText(peak_txt, ro.removeFromTop(line_h),
                   juce::Justification::centredRight);

        auto lufsLine = [&](const char* label, float v) {
            const juce::String txt = (v <= -99.0f)
                ? juce::String(label) + " --"
                : juce::String(label) + " " + juce::String(v, 1);
            g.drawText(txt, ro.removeFromTop(line_h),
                       juce::Justification::centredRight);
        };
        g.setColour(juce::Colours::white.withAlpha(0.55f));
        g.setFont(juce::FontOptions(10.0f));
        lufsLine("I:", lufs_integrated_);
        lufsLine("M:", lufs_momentary_);
        lufsLine("S:", lufs_short_term_);
    }
}

void LayerRow::resized() {
    auto strip = stripArea();

    // Move column — small up/down buttons stacked vertically.
    {
        auto move_col = strip.removeFromLeft(kMoveColumnWidth);
        const int btn_h = move_col.getHeight() / 2 - 1;
        move_up_button_  .setBounds(move_col.removeFromTop(btn_h).reduced(1));
        move_col.removeFromTop(2);
        move_down_button_.setBounds(move_col.removeFromTop(btn_h).reduced(1));
    }

    strip.removeFromLeft(kColorTagWidth);

    // Info
    auto info = strip.removeFromLeft(kInfoAreaWidth).reduced(8, 4);
    track_.setBounds(info.removeFromTop(22));
    name_ .setBounds(info);
    strip.removeFromLeft(kSectionGap);

    const int btn_y = strip.getY() + (strip.getHeight() - kButtonSize) / 2;

    // R (record-arm)
    record_button_.setBounds(strip.removeFromLeft(kButtonSize)
                                  .withY(btn_y).withHeight(kButtonSize));
    strip.removeFromLeft(kSectionGap);

    // NORM area: [target field] [N button] [db readout], vertically centred
    {
        auto norm = strip.removeFromLeft(kNormAreaWidth);
        const int norm_h = 18;
        const int norm_y = norm.getY() + (norm.getHeight() - norm_h) / 2;
        int nx = norm.getX();
        norm_target_label_.setBounds(nx, norm_y, kNormTargetW, norm_h);
        nx += kNormTargetW + kNormInnerGap;
        normalize_button_.setBounds(nx, btn_y, kButtonSize, kButtonSize);
        nx += kButtonSize + kNormInnerGap;
        norm_db_label_.setBounds(nx, norm_y, kNormDbW, norm_h);
    }
    strip.removeFromLeft(kSectionGap);

    // Volume fader
    {
        auto fader = strip.removeFromLeft(kFaderWidth);
        const int fader_h = 22;
        gain_slider_.setBounds(fader.getX(),
                                fader.getY() + (fader.getHeight() - fader_h) / 2,
                                fader.getWidth(),
                                fader_h);
    }
    strip.removeFromLeft(kSectionGap);

    // M / S
    mute_button_.setBounds(strip.removeFromLeft(kButtonSize)
                                .withY(btn_y).withHeight(kButtonSize));
    strip.removeFromLeft(kSectionGap);
    solo_button_.setBounds(strip.removeFromLeft(kButtonSize)
                                .withY(btn_y).withHeight(kButtonSize));

    // Meters + readout positioned by the helpers (they don't need explicit setBounds).

    lane_.setBounds(laneArea());
}

juce::Rectangle<int> LayerRow::stripArea() const {
    return getLocalBounds().removeFromLeft(kStripFixedWidth);
}

juce::Rectangle<int> LayerRow::meterAreaL() const {
    auto strip = stripArea();
    // Skip everything up to (and including) the S button + its trailing gap.
    const int meters_x = strip.getX() + kMoveColumnWidth
                       + kColorTagWidth + kInfoAreaWidth + kSectionGap
                       + kButtonSize + kSectionGap
                       + kNormAreaWidth + kSectionGap
                       + kFaderWidth + kSectionGap
                       + kButtonSize + kSectionGap
                       + kButtonSize + kSectionGap;
    const int total = kMeterBarHeight * 2 + kMeterBarGap;
    const int top   = strip.getY() + (strip.getHeight() - total) / 2;
    return { meters_x, top, kMeterWidth, kMeterBarHeight };
}

juce::Rectangle<int> LayerRow::meterAreaR() const {
    auto L = meterAreaL();
    return L.translated(0, kMeterBarHeight + kMeterBarGap);
}

juce::Rectangle<int> LayerRow::readoutArea() const {
    auto strip = stripArea();
    strip.removeFromRight(kStripPadRight);
    return strip.removeFromRight(kReadoutWidth);
}

juce::Rectangle<int> LayerRow::laneArea() const {
    auto bounds = getLocalBounds();
    bounds.removeFromLeft(kStripFixedWidth);
    return bounds.reduced(2, 2);
}

void LayerRow::ArrowButton::paintButton(juce::Graphics& g, bool over, bool down) {
    auto bounds = getLocalBounds().toFloat();

    const auto bg_alpha = isEnabled()
        ? (down ? 0.55f : (over ? 0.45f : 0.32f))
        : 0.15f;
    g.setColour(juce::Colours::black.withAlpha(bg_alpha));
    g.fillRoundedRectangle(bounds, 2.0f);

    const float cx     = bounds.getCentreX();
    const float cy     = bounds.getCentreY();
    const float side   = std::min(bounds.getWidth(), bounds.getHeight()) * 0.55f;
    const float half_w = side * 0.5f;
    const float half_h = side * 0.45f;

    juce::Path tri;
    if (up_) {
        tri.startNewSubPath(cx,          cy - half_h);
        tri.lineTo         (cx - half_w, cy + half_h);
        tri.lineTo         (cx + half_w, cy + half_h);
    } else {
        tri.startNewSubPath(cx,          cy + half_h);
        tri.lineTo         (cx - half_w, cy - half_h);
        tri.lineTo         (cx + half_w, cy - half_h);
    }
    tri.closeSubPath();

    g.setColour(isEnabled() ? juce::Colours::white
                              : juce::Colours::white.withAlpha(0.3f));
    g.fillPath(tri);
}

float LayerRow::dbToFraction(float db) const {
    if (db <= kMeterDbMin) return 0.0f;
    if (db >= kMeterDbMax) return 1.0f;
    return (db - kMeterDbMin) / (kMeterDbMax - kMeterDbMin);
}

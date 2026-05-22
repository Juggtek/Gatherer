#pragma once

#include <JuceHeader.h>

#include <cstdint>
#include <functional>

#include "LayerLane.h"

// One layer row, laid out horizontally like a DAW track.
//
//   ┌──────────────────────────────────────────────────────────────────────────┐
//   │  channel strip (fixed width)              │   waveform lane (flex)        │
//   │  [color][name/track][M S R][gain][meters] │   [thumbnail of recorded WAV] │
//   └──────────────────────────────────────────────────────────────────────────┘
//
// L/R meters are stacked thin bars on the right side of the strip; the lane
// hosts a LayerLane child component bound to a juce::AudioThumbnail.
class LayerRow : public juce::Component {
public:
    LayerRow();

    void setIdentity(const juce::String& display_name,
                     const juce::String& track_name,
                     std::uint32_t       color_rgba);

    // Per-channel levels in dB.
    void setLevels(float peak_db_l, float rms_db_l,
                   float peak_db_r, float rms_db_r);

    // EBU R128 LUFS snapshot (integrated / momentary / short-term).
    void setLufs(float integrated, float momentary, float short_term);

    // Reflect current mixer state from the processor without firing callbacks.
    void setMixState(bool mute, bool solo, bool record_arm,
                     float gain_db, float norm_db, float target_lufs);

    // Bind the waveform thumbnail (owned elsewhere).
    void setThumbnail(juce::AudioThumbnail* thumbnail) { lane_.setThumbnail(thumbnail); }

    // Toggle whether the lane's "Delete recording" right-click item is enabled.
    void setRecordingAvailable(bool available) noexcept { lane_.setRecordingAvailable(available); }

    // Drive the playhead drawing on the lane. Both are global values; the row
    // maps them to a [0,1] fraction across the lane width.
    void setPlayheadSeconds(double s)        { playhead_seconds_ = s; updatePlayhead(); }
    void setSessionLengthSeconds(double s)   { session_seconds_  = s; updatePlayhead(); updateGrid(); }
    void setGridBpm(double bpm)              { grid_bpm_     = bpm; updateGrid(); }
    void setGridTimeSigNum(int n)            { grid_tsn_     = n;   updateGrid(); }
    void setGridStartInBeats(double b)       { grid_start_b_ = b;   updateGrid(); }
    void setSlotLayout(double offset_seconds, double length_seconds) {
        lane_.setSlotLayout(offset_seconds, length_seconds);
    }

    // Interaction callbacks
    std::function<void(bool)>  onMuteChanged;
    std::function<void(bool)>  onSoloChanged;
    std::function<void(bool)>  onRecordArmChanged;
    std::function<void(float)> onGainDbChanged;
    std::function<void()>      onNormalize;
    std::function<void(float)> onTargetLufsChanged;
    std::function<void()>      onDeleteRecording;
    std::function<void(double)> onSeekSeconds;
    std::function<void()>      onMoveUp;
    std::function<void()>      onMoveDown;

    // Disable up/down buttons at the top/bottom of the display order.
    void setMoveUpEnabled(bool e)   { move_up_button_  .setEnabled(e); }
    void setMoveDownEnabled(bool e) { move_down_button_.setEnabled(e); }

    void paint(juce::Graphics&) override;
    void resized() override;

private:
    juce::Label         name_;
    juce::Label         track_;
    juce::TextButton    mute_button_      { "M" };
    juce::TextButton    solo_button_      { "S" };
    juce::TextButton    record_button_    { "R" };
    juce::TextButton    normalize_button_ { "N" };

    // Custom up/down triangle buttons — clearer than text "^" / "v" at the
    // small column width the reorder controls live in.
    class ArrowButton : public juce::Button {
    public:
        ArrowButton(const juce::String& name, bool points_up)
            : juce::Button(name), up_(points_up) {}
        void paintButton(juce::Graphics& g, bool over, bool down) override;
    private:
        bool up_;
    };
    ArrowButton move_up_button_   { "move_up",   true  };
    ArrowButton move_down_button_ { "move_down", false };
    juce::Label         norm_target_label_;   // editable: per-slot target LUFS
    juce::Label         norm_db_label_;       // readonly: current normalize gain in dB
    juce::Slider        gain_slider_;
    LayerLane           lane_;

    juce::Colour        color_           { juce::Colours::cornflowerblue };
    float               peak_db_l_       { -100.0f };
    float               rms_db_l_        { -100.0f };
    float               peak_db_r_       { -100.0f };
    float               rms_db_r_        { -100.0f };
    float               lufs_integrated_ { -100.0f };
    float               lufs_momentary_  { -100.0f };
    float               lufs_short_term_ { -100.0f };
    double              playhead_seconds_ { 0.0 };
    double              session_seconds_  { 0.0 };
    double              grid_bpm_         { 0.0 };
    int                 grid_tsn_         { 4 };
    double              grid_start_b_     { 0.0 };

    void                updatePlayhead();
    void                updateGrid();

    static constexpr float kMeterDbMin = -60.0f;
    static constexpr float kMeterDbMax =   3.0f;

    juce::Rectangle<int> meterAreaL() const;
    juce::Rectangle<int> meterAreaR() const;
    juce::Rectangle<int> readoutArea() const;
    juce::Rectangle<int> laneArea() const;
    juce::Rectangle<int> stripArea() const;
    float                dbToFraction(float db) const;
};

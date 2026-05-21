#pragma once

#include <JuceHeader.h>

#include <functional>

// Right-hand half of a LayerRow: the waveform display ("lane"), like a clip
// strip in a DAW. Reads from a juce::AudioThumbnail (owned by the processor)
// and repaints when it changes. The lane scrolls left-to-right as recording
// proceeds; total visible range grows with the thumbnail length.
class LayerLane : public juce::Component, private juce::ChangeListener {
public:
    LayerLane();
    ~LayerLane() override;

    // Bind to a thumbnail (lifetime owned elsewhere; pass nullptr to detach).
    void setThumbnail(juce::AudioThumbnail* thumbnail);

    // Fired when the user picks "Delete recording" from the right-click menu.
    // The owning LayerRow forwards this; only enabled when a recording exists.
    std::function<void()> onDeleteRecording;

    // Fired when the user left-clicks (or drags) inside the lane to scrub the
    // global playhead. `fraction` is in [0, 1] across the lane width.
    std::function<void(double fraction)> onSeekFraction;

    // Whether to enable the delete menu item (owner sets based on whether
    // there's a recording on disk for this slot).
    void setRecordingAvailable(bool available) noexcept { has_recording_ = available; }

    // Playhead position to draw, as a fraction of the lane width in [0, 1].
    // The owning row maps from global playhead time / session length.
    void setPlayheadFraction(double f);

    // Beat grid. bpm > 0 enables drawing; time_sig_num drives the downbeat
    // accent. session_start_in_beats is the DAW PPQ at the *session's* x=0
    // (the earliest start across all recorded slots). session_seconds is the
    // session's total duration. Both shared across every lane so beat lines
    // line up vertically across the layer stack.
    void setGrid(double bpm, int time_sig_num, double session_start_in_beats,
                 double session_seconds);

    // Where this slot's audio sits within the session timeline. Both in
    // seconds, both >= 0. Audio is drawn from
    //   x_start = (slot_offset / session_seconds) * lane_width
    //   x_end   = ((slot_offset + slot_length) / session_seconds) * lane_width
    // Slots that started later than the session's reference appear pushed
    // right in the lane, with empty space to their left.
    void setSlotLayout(double slot_offset_seconds, double slot_length_seconds);

    void paint     (juce::Graphics&) override;
    void mouseDown (const juce::MouseEvent&) override;
    void mouseDrag (const juce::MouseEvent&) override;

private:
    void changeListenerCallback(juce::ChangeBroadcaster*) override;

    juce::AudioThumbnail* thumbnail_         = nullptr;
    bool                  has_recording_     = false;
    double                playhead_fraction_ = 0.0;

    double                grid_bpm_           = 0.0;
    int                   grid_time_sig_num_  = 4;
    double                grid_start_beats_   = 0.0;
    double                grid_session_secs_  = 0.0;

    double                slot_offset_secs_   = 0.0;
    double                slot_length_secs_   = 0.0;
};

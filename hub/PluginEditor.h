#pragma once

#include <JuceHeader.h>

#include <memory>
#include <vector>

#include "PluginProcessor.h"
#include "LayerRow.h"
#include "diagnostics/HealthMonitor.h"

class HubEditor : public juce::AudioProcessorEditor,
                  private juce::Timer {
public:
    explicit HubEditor(HubProcessor&);
    ~HubEditor() override;

    void paint(juce::Graphics&) override;
    void resized() override;
    bool keyPressed(const juce::KeyPress& k) override;

private:
    void timerCallback() override;
    void updateHealth();
    void refreshUndoRedoButtons();

    HubProcessor&     processor_;
    juce::Label       title_;
    juce::Label       status_;
    juce::ToggleButton include_input_toggle_ { "Include track input as source" };
    juce::ToggleButton pad_silence_toggle_   { "Pad silence in recording" };

    juce::TextButton  master_record_button_ { "● Record" };
    juce::Label       record_status_;

    juce::Label       global_target_caption_;
    juce::Label       global_target_label_;       // editable LUFS target (global default)
    juce::TextButton  normalize_all_button_   { "Normalize All" };
    juce::TextButton  export_normalized_button_ { "Export Normalized" };

    juce::TextButton  undo_button_ { "Undo" };
    juce::TextButton  redo_button_ { "Redo" };

    juce::TextButton  play_button_ { "Play" };
    juce::TextButton  stop_button_ { "Stop" };
    juce::Label       transport_pos_;

    juce::Label       session_caption_;
    juce::Label       session_name_;
    juce::TextButton  session_new_button_  { "New" };
    juce::TextButton  session_open_button_ { "Open" };
    juce::TextButton  session_save_button_ { "Save" };
    std::unique_ptr<juce::FileChooser> session_chooser_;

    // Health panel.
    juce::Label       health_summary_;
    juce::Label       health_detail_;
    juce::TextButton  health_reset_button_ { "Re-analyze" };
    juce::Colour      health_badge_color_ { juce::Colours::grey };

    // Calibration probe panel.
    juce::TextButton  calibrate_button_ { "Calibrate" };
    juce::Label       calibrate_summary_;
    juce::Label       calibrate_detail_;
    juce::Colour      calibrate_badge_color_ { juce::Colours::grey };

    // One row per slot, allocated on demand as sats appear. Hidden when the slot
    // is no longer active. Stacked vertically inside layers_container_.
    std::array<std::unique_ptr<LayerRow>, gatherer::protocol::NUM_SLOTS> rows_{};
    juce::Component                                                     layers_container_;
    std::size_t                                                         last_layout_count_ = 0;

    gatherer::diagnostics::HealthMonitor health_monitor_;

    bool was_normalizing_ = false;
};

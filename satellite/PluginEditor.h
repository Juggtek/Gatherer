#pragma once

#include <JuceHeader.h>

#include "PluginProcessor.h"

class SatelliteEditor : public juce::AudioProcessorEditor,
                        private juce::Timer {
public:
    explicit SatelliteEditor(SatelliteProcessor&);
    ~SatelliteEditor() override = default;

    void paint(juce::Graphics&) override;
    void resized() override;

private:
    void timerCallback() override;

    SatelliteProcessor& processor_;
    juce::Label         title_;
    juce::Label         slot_label_;
    juce::Label         uuid_label_;
    juce::Label         track_label_;
    juce::Label         hub_label_;
};

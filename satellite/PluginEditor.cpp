#include "PluginEditor.h"

SatelliteEditor::SatelliteEditor(SatelliteProcessor& p)
    : juce::AudioProcessorEditor(&p), processor_(p)
{
    auto setupLabel = [this](juce::Label& l, const juce::String& text, float fontSize, juce::Justification j) {
        l.setText(text, juce::dontSendNotification);
        l.setFont(juce::FontOptions(fontSize));
        l.setJustificationType(j);
        addAndMakeVisible(l);
    };

    setupLabel(title_,       "Gatherer Satellite", 18.0f, juce::Justification::centred);
    setupLabel(slot_label_,  "slot: ?",            14.0f, juce::Justification::centredLeft);
    setupLabel(uuid_label_,  "uuid: ?",            12.0f, juce::Justification::centredLeft);
    setupLabel(track_label_, "track: ?",           12.0f, juce::Justification::centredLeft);
    setupLabel(hub_label_,   "hub: ?",             14.0f, juce::Justification::centredLeft);

    setSize(320, 180);
    startTimerHz(15);
}

void SatelliteEditor::paint(juce::Graphics& g) {
    g.fillAll(juce::Colours::darkslategrey);
}

void SatelliteEditor::resized() {
    auto area = getLocalBounds().reduced(12);
    title_.setBounds(area.removeFromTop(30));
    area.removeFromTop(6);
    slot_label_.setBounds (area.removeFromTop(22));
    uuid_label_.setBounds (area.removeFromTop(22));
    track_label_.setBounds(area.removeFromTop(22));
    hub_label_.setBounds  (area.removeFromTop(22));
}

void SatelliteEditor::timerCallback() {
    const int slot = processor_.getSlotIndex();
    const auto uuid = processor_.getUuid();
    const auto track = processor_.getTrackName();

    slot_label_.setText(slot >= 0 ? ("slot: " + juce::String(slot))
                                  : juce::String("slot: NOT CLAIMED"),
                        juce::dontSendNotification);
    uuid_label_.setText("uuid: " + juce::String::toHexString(static_cast<juce::int64>(uuid)).paddedLeft('0', 16),
                        juce::dontSendNotification);
    track_label_.setText("track: " + (track.isEmpty() ? juce::String("<host did not report>") : track),
                         juce::dontSendNotification);
    hub_label_.setText(processor_.isHubConnected() ? "hub: CONNECTED" : "hub: not present",
                       juce::dontSendNotification);
}

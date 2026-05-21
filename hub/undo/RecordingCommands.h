#pragma once

#include <JuceHeader.h>

#include "Command.h"

class HubProcessor;

namespace gatherer::undo {

// Soft-deletes a recording WAV (and its `_normalized.wav` sibling, if any) to a
// `.trash/` subfolder of the session. Undo moves them back. Also clears /
// rebuilds the layer's thumbnail so the lane reflects the change visually.
class DeleteRecordingCommand : public Command {
public:
    DeleteRecordingCommand(HubProcessor& p, int slot, juce::File wav);

    void         apply()    override;
    void         revert()   override;
    juce::String describe() const override { return "Delete Recording"; }

private:
    void        clearThumbnail() const;
    void        restoreThumbnail() const;
    juce::File  trashFor(const juce::File& f) const;

    HubProcessor* p_;
    int           slot_;
    juce::File    wav_;
    juce::File    normalized_;  // sibling _normalized.wav, may not exist
};

}  // namespace gatherer::undo

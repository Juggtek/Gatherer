#include "RecordingCommands.h"

#include "../PluginProcessor.h"

namespace gatherer::undo {

namespace {
juce::File makeTrashDir(const juce::File& wav) {
    auto d = wav.getParentDirectory().getChildFile(".trash");
    d.createDirectory();
    return d;
}
}  // namespace

DeleteRecordingCommand::DeleteRecordingCommand(HubProcessor& p, int slot, juce::File wav)
    : p_(&p), slot_(slot), wav_(std::move(wav))
{
    normalized_ = wav_.getSiblingFile(wav_.getFileNameWithoutExtension() + "_normalized.wav");
}

juce::File DeleteRecordingCommand::trashFor(const juce::File& f) const {
    return makeTrashDir(wav_).getChildFile(f.getFileName());
}

void DeleteRecordingCommand::clearThumbnail() const {
    if (auto* tn = p_->getThumbnail(slot_)) tn->reset(0, 0);
}

void DeleteRecordingCommand::restoreThumbnail() const {
    if (!wav_.existsAsFile()) return;
    if (auto* tn = p_->getThumbnail(slot_)) {
        tn->reset(0, 0);
        tn->setSource(new juce::FileInputSource(wav_));
    }
}

void DeleteRecordingCommand::apply() {
    if (wav_.existsAsFile()) {
        wav_.moveFileTo(trashFor(wav_));
    }
    if (normalized_.existsAsFile()) {
        normalized_.moveFileTo(trashFor(normalized_));
    }
    p_->setLastRecordingForSlot(slot_, juce::File{});
    p_->playback().setSourceForSlot(slot_, juce::File{});  // drop the playback source
    clearThumbnail();
    p_->session().autoSave();
}

void DeleteRecordingCommand::revert() {
    const auto wav_trash  = trashFor(wav_);
    const auto norm_trash = trashFor(normalized_);
    if (wav_trash.existsAsFile())  wav_trash .moveFileTo(wav_);
    if (norm_trash.existsAsFile()) norm_trash.moveFileTo(normalized_);
    p_->setLastRecordingForSlot(slot_, wav_);
    p_->playback().setSourceForSlot(slot_, wav_);          // re-arm playback
    restoreThumbnail();
    p_->session().autoSave();
}

}  // namespace gatherer::undo

#include "Recorder.h"

Recorder::Recorder(gatherer::protocol::SharedRegion* region)
    : region_(region) {}

Recorder::~Recorder() {
    stop();
}

bool Recorder::start(const std::vector<ArmedLayer>& armed, double sample_rate,
                     juce::File session_folder) {
    if (recording_.load(std::memory_order_acquire)) return false;
    if (armed.empty() || region_ == nullptr || sample_rate <= 0.0) return false;
    if (session_folder == juce::File{}) return false;

    session_folder_ = std::move(session_folder);
    if (!session_folder_.exists()) {
        const auto res = session_folder_.createDirectory();
        if (res.failed()) return false;
    }

    writers_.clear();
    writers_.reserve(armed.size());

    for (const auto& a : armed) {
        const auto safe_track = juce::File::createLegalFileName(a.track_name);
        const auto safe_disp  = juce::File::createLegalFileName(a.display_name);
        const auto file_name  = juce::String::formatted("slot%02d_%s_%s.wav",
                                                          a.slot,
                                                          safe_track.toRawUTF8(),
                                                          safe_disp.toRawUTF8());
        auto file   = session_folder_.getChildFile(file_name);
        auto writer = std::make_unique<LayerWriter>(a.slot, region_, file,
                                                      sample_rate, a.start_wp,
                                                      a.thumbnail);
        if (!writer->prepare()) {
            // Skip this layer if its writer can't be set up.
            continue;
        }
        writer->startThread();
        writers_.push_back(std::move(writer));
    }

    if (writers_.empty()) {
        // Nothing usable — clean up the empty session folder.
        session_folder_.deleteRecursively();
        return false;
    }

    recording_.store(true, std::memory_order_release);
    return true;
}

void Recorder::stop() {
    if (!recording_.load(std::memory_order_acquire)) return;
    // Each writer's stopAndFinalize drains remaining samples, then closes the file.
    // Run them in parallel implicitly — each writer's thread shuts down independently.
    for (auto& w : writers_) {
        w->signalThreadShouldExit();
    }
    for (auto& w : writers_) {
        w->stopAndFinalize();
    }
    writers_.clear();
    recording_.store(false, std::memory_order_release);
}

std::vector<Recorder::WriterStatus> Recorder::writerStatuses() const {
    std::vector<WriterStatus> out;
    out.reserve(writers_.size());
    for (const auto& w : writers_) {
        WriterStatus s;
        s.slot            = w->slotIndex();
        s.file            = w->outputFile();
        s.samples_written = w->samplesWritten();
        out.push_back(s);
    }
    return out;
}

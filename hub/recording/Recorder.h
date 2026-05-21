#pragma once

#include <JuceHeader.h>

#include <atomic>
#include <memory>
#include <vector>

#include "LayerWriter.h"
#include "protocol/SharedRegion.h"

// Coordinates a recording session: when started, spawns one LayerWriter per
// armed slot into a timestamped folder under ~/Documents/Gatherer Recordings/.
// Stop signals all writers to drain and finalize their WAVs.
//
// Phase R1: no manifest, no session metadata, no thumbnails. Just files on disk.
class Recorder {
public:
    explicit Recorder(gatherer::protocol::SharedRegion* region);
    ~Recorder();

    struct ArmedLayer {
        int                   slot;
        juce::String          track_name;
        juce::String          display_name;
        juce::AudioThumbnail* thumbnail = nullptr;  // optional; if non-null, fed live
        // sat.write_pos snapshot taken by the caller right before start(); used
        // as the writer's first-sample position so it agrees byte-accurately
        // with the hub's grid-capture reference (no writer-side re-snapshot).
        std::uint64_t         start_wp = 0;
    };

    // Returns true on success. False if already recording, no slots armed,
    // or the session folder could not be created. The session folder is
    // supplied by the caller (SessionManager); writes happen directly inside
    // it. WAVs are named slot{NN}_{track}_{sat}.wav.
    bool start(const std::vector<ArmedLayer>& armed, double sample_rate,
               juce::File session_folder);
    void stop();
    bool isRecording() const noexcept { return recording_.load(std::memory_order_acquire); }

    juce::File currentSessionFolder() const { return session_folder_; }
    int        numActiveWriters()     const noexcept
        { return static_cast<int>(writers_.size()); }

    struct WriterStatus {
        int           slot;
        juce::File    file;
        std::uint64_t samples_written;
    };
    std::vector<WriterStatus> writerStatuses() const;

private:
    gatherer::protocol::SharedRegion*         region_;
    std::atomic<bool>                         recording_ { false };
    juce::File                                session_folder_;
    std::vector<std::unique_ptr<LayerWriter>> writers_;
};

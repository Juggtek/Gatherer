#pragma once

#include <JuceHeader.h>

#include <array>
#include <atomic>
#include <cstdint>
#include <memory>

#include "protocol/SharedRegion.h"

namespace gatherer::playback {

// Multi-track WAV playback engine for the hub.
//
// Threading model:
//   - Message thread: setSourceForSlot, clearSources, play/pause/stop, seek
//   - Audio thread:   readSlotIntoInterleaved (one call per slot per block)
//                     then advancePlayhead once
//
// Refreshing sources mutates the array; we guarantee transport is Stopped before
// any source pointer changes (callers must respect this — see HubProcessor::
// stopRecording / refreshPlaybackSources). When transport is Stopped, the audio
// thread does not touch sources_, so plain unique_ptr swaps are safe.
class Engine {
public:
    enum class State { Stopped, Playing, Paused };

    Engine();
    ~Engine();

    // Prepare for audio. Must be called from the audio thread (or before audio
    // starts). The sample rate is needed because reader sample rates may differ
    // from the host — we resample if so. For now we require matching rates and
    // skip mismatched sources.
    void prepare(double host_sample_rate, int max_block_size);
    void release();

    // ----- Sources -----

    // Load a WAV for a slot. Pass an invalid File to clear.
    void setSourceForSlot(int slot, const juce::File& wav);
    void clearAll();

    // True if this slot has a loaded, ready-to-play source.
    bool hasSourceForSlot(int slot) const noexcept;

    // Total length of this slot's source in samples (0 if none).
    std::int64_t slotLengthSamples(int slot) const noexcept;

    // Per-slot offset within the session timeline. Used to align slots that
    // were recorded at different DAW positions: at session-playhead `p`,
    // this slot reads its file at sample (p − offset). Defaults to 0.
    void         setSlotOffsetSamples(int slot, std::int64_t offset);
    std::int64_t slotOffsetSamples(int slot) const noexcept;

    // Max (offset + length) across all loaded slots == "session length".
    std::int64_t sessionLengthSamples() const noexcept;
    double       sessionLengthSeconds() const noexcept;

    // ----- Transport -----

    State        state() const noexcept { return static_cast<State>(state_.load(std::memory_order_acquire)); }
    bool         isPlaying() const noexcept { return state() == State::Playing; }
    void         play();
    void         pause();
    void         stop();              // pauses + rewinds to 0
    void         seekSamples(std::int64_t pos);
    void         seekSeconds(double s);

    std::int64_t playheadSamples() const noexcept { return playhead_.load(std::memory_order_acquire); }
    double       playheadSeconds() const noexcept;

    double       hostSampleRate()  const noexcept { return sample_rate_; }

    // ----- Audio thread API -----

    // Read up to `frames` of interleaved stereo audio from slot's source,
    // starting at the engine's current playhead position. Returns true if data
    // was produced; false if no source / past end / not playing. The buffer
    // must be at least frames * 2 floats long.
    bool readSlotIntoInterleaved(int slot, float* dest, int frames);

    // Advance the playhead by `frames`. If we cross sessionLengthSamples(),
    // auto-stops the transport.
    void advancePlayhead(int frames);

private:
    struct Source {
        std::unique_ptr<juce::AudioFormatReader> reader;
        std::int64_t                              length         = 0;
        std::int64_t                              offset_samples = 0;
    };

    juce::AudioFormatManager                              fmt_manager_;
    std::array<Source, gatherer::protocol::NUM_SLOTS>     sources_;
    juce::AudioBuffer<float>                              scratch_planar_;  // reused per-read

    double                                                sample_rate_      = 0.0;
    int                                                   max_block_size_   = 0;
    std::atomic<int>                                      state_            { 0 };  // 0=Stopped 1=Playing 2=Paused
    std::atomic<std::int64_t>                             playhead_         { 0 };
    std::atomic<std::int64_t>                             session_length_   { 0 };

    void recomputeSessionLength();
};

}  // namespace gatherer::playback

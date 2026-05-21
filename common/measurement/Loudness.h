#pragma once

// EBU R128 loudness analyzer wrapper around libebur128. Header-only RAII.
// Single-threaded: the same instance must not be fed from multiple threads
// simultaneously. Pattern in Gatherer: feed from the audio thread, also query
// from the audio thread at a slower cadence, and atomically publish the
// results for the GUI thread to read.

#include <ebur128.h>

#include <cstddef>

namespace gatherer::measurement {

class LoudnessAnalyzer {
public:
    LoudnessAnalyzer(int channels, double sample_rate)
        : channels_(channels), sample_rate_(sample_rate),
          state_(ebur128_init(static_cast<unsigned int>(channels),
                              static_cast<unsigned long>(sample_rate),
                              EBUR128_MODE_I | EBUR128_MODE_M | EBUR128_MODE_S)) {}

    ~LoudnessAnalyzer() {
        if (state_) ebur128_destroy(&state_);
    }

    LoudnessAnalyzer(const LoudnessAnalyzer&) = delete;
    LoudnessAnalyzer& operator=(const LoudnessAnalyzer&) = delete;

    bool valid() const noexcept { return state_ != nullptr; }

    void addInterleavedFloat(const float* data, std::size_t frames) noexcept {
        if (state_) ebur128_add_frames_float(state_, data, frames);
    }

    void reset() noexcept {
        if (state_) ebur128_destroy(&state_);
        state_ = ebur128_init(static_cast<unsigned int>(channels_),
                              static_cast<unsigned long>(sample_rate_),
                              EBUR128_MODE_I | EBUR128_MODE_M | EBUR128_MODE_S);
    }

    // -INFINITY (< -100) when no data has been added or below absolute gate.
    double integratedLufs() const noexcept {
        double v = -100.0;
        if (state_) ebur128_loudness_global(const_cast<ebur128_state*>(state_), &v);
        return v;
    }
    double momentaryLufs() const noexcept {
        double v = -100.0;
        if (state_) ebur128_loudness_momentary(const_cast<ebur128_state*>(state_), &v);
        return v;
    }
    double shortTermLufs() const noexcept {
        double v = -100.0;
        if (state_) ebur128_loudness_shortterm(const_cast<ebur128_state*>(state_), &v);
        return v;
    }

private:
    int            channels_;
    double         sample_rate_;
    ebur128_state* state_ = nullptr;
};

} // namespace gatherer::measurement

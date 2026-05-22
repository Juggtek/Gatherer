#pragma once

#include <JuceHeader.h>

#include <atomic>
#include <memory>
#include <vector>

// Offline normalization writer: takes a list of (file, gain_db) tasks and
// renders `*_normalized.wav` siblings on a background thread.
//
// Gain is supplied per task (not computed here) so the live preview state — the
// per-slot `norm_db` the user dialed in via the N / Normalize All buttons — is
// the source of truth. This keeps the rendered file in lockstep with what the
// user is hearing at the moment they choose to export.
class OfflineNormalizer : private juce::Thread {
public:
    struct Task {
        juce::File   file;
        float        gain_db              = 0.0f;
        juce::String output_suffix        = "_normalized";  // appended before .wav
        // Optional session-alignment fields. When total_length_samples > 0 the
        // output is padded with leading silence of `offset_samples` followed by
        // the gained source, followed by trailing silence to reach exactly
        // total_length_samples. When total_length_samples == 0 the output is
        // the same length as the source (no padding).
        std::int64_t offset_samples       = 0;
        std::int64_t total_length_samples = 0;
    };

    struct Result {
        juce::File   source;
        juce::File   output;
        float        gain_applied_db = 0.0f;
        bool         success         = false;
        juce::String error;
    };

    explicit OfflineNormalizer(std::vector<Task> tasks);
    ~OfflineNormalizer() override;

    void startAsync() { startThread(); }
    bool inProgress() const noexcept { return in_progress_.load(std::memory_order_acquire); }

    // Snapshot of results so far. Safe to call from any thread; while running
    // the vector grows as files finish.
    std::vector<Result> results() const;

private:
    void run() override;

    std::vector<Task>             tasks_;
    mutable juce::CriticalSection results_lock_;
    std::vector<Result>           results_;
    std::atomic<bool>             in_progress_ { false };
};

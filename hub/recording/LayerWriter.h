#pragma once

#include <JuceHeader.h>

#include <atomic>
#include <cstdint>
#include <memory>

#include "protocol/SharedRegion.h"

// One per active recording layer. Owns a background thread that drains the
// satellite's ring buffer at ~50Hz with large chunks and writes a WAV file
// via JUCE's standard AudioFormatWriter.
//
// Recording is **pre-fader** — reads come straight from the satellite's ring,
// independent of hub-side mute / solo / gain.
class LayerWriter : public juce::Thread {
public:
    LayerWriter(int slot_index,
                gatherer::protocol::SharedRegion* region,
                const juce::File& output_file,
                double sample_rate,
                std::uint64_t start_wp,
                juce::AudioThumbnail* thumbnail = nullptr,
                std::atomic<std::uint64_t>* expected_samples = nullptr);
    ~LayerWriter() override;

    // Opens the output file. The recording start position (sat.write_pos at
    // record-time) is supplied via the constructor — the caller (HubProcessor)
    // takes the wp snapshot for every armed slot in one tight loop *before*
    // any prepare() runs, so all writers and the grid math agree on the same
    // sample-accurate start position regardless of file-open latency.
    bool prepare();

    // Signals the writer to stop, drains any remaining samples up to the
    // satellite's current wp, finalizes the WAV. Blocks up to ~2s.
    void stopAndFinalize();

    int               slotIndex()      const noexcept { return slot_; }
    const juce::File& outputFile()     const noexcept { return file_; }
    std::uint64_t     samplesWritten() const noexcept
        { return samples_written_.load(std::memory_order_relaxed); }

private:
    void run() override;
    void drainChunk(std::uint32_t frames, std::vector<float>& interleaved,
                    juce::AudioBuffer<float>& planar);
    void writeSilence(std::uint32_t frames, juce::AudioBuffer<float>& planar);

    int                                       slot_;
    gatherer::protocol::SharedRegion*         region_;
    juce::File                                file_;
    double                                    sample_rate_;
    juce::AudioThumbnail*                     thumbnail_      = nullptr;
    std::uint64_t                             start_wp_       = 0;
    std::uint64_t                             read_pos_       = 0;
    std::atomic<std::uint64_t>                samples_written_ { 0 };
    std::atomic<std::uint64_t>*               expected_samples_ = nullptr;  // optional padding sink
    std::unique_ptr<juce::AudioFormatWriter>  writer_;
};

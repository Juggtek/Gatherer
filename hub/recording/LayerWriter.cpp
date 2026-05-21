#include "LayerWriter.h"

#include "ringbuffer/SpscRingBuffer.h"

using namespace gatherer;
using namespace gatherer::protocol;

namespace {
constexpr std::uint32_t kChunkFrames = 4096;     // ~85ms at 48k — well under ring capacity
constexpr int           kWaitMs      = 20;        // background poll cadence
constexpr int           kStopTimeout = 2000;
}

LayerWriter::LayerWriter(int slot,
                         SharedRegion* region,
                         const juce::File& file,
                         double sr,
                         std::uint64_t start_wp,
                         juce::AudioThumbnail* thumbnail,
                         std::atomic<std::uint64_t>* expected_samples)
    : juce::Thread("Gatherer LayerWriter " + juce::String(slot))
    , slot_(slot)
    , region_(region)
    , file_(file)
    , sample_rate_(sr)
    , thumbnail_(thumbnail)
    , start_wp_(start_wp)
    , read_pos_(start_wp)
    , expected_samples_(expected_samples) {}

LayerWriter::~LayerWriter() {
    stopAndFinalize();
}

bool LayerWriter::prepare() {
    if (region_ == nullptr || sample_rate_ <= 0.0) return false;

    file_.deleteFile();
    auto stream = std::make_unique<juce::FileOutputStream>(file_);
    if (!stream->openedOk()) return false;

    juce::WavAudioFormat fmt;
    // 32-bit float, stereo. WAV's quality arg is unused.
    writer_.reset(fmt.createWriterFor(stream.release(),
                                       sample_rate_,
                                       static_cast<unsigned int>(RING_CHANNELS),
                                       /*bits per sample*/ 32,
                                       {},
                                       /*quality*/ 0));
    if (!writer_) {
        file_.deleteFile();
        return false;
    }

    // start_wp_ / read_pos_ were set in the constructor from the caller's
    // pre-snapshotted wp. Don't re-snapshot here — that would race with the
    // sat audio thread and (worse) produce per-writer drift when file-open
    // latency lets several audio blocks fire between consecutive prepares.

    if (thumbnail_ != nullptr) {
        thumbnail_->reset(static_cast<int>(RING_CHANNELS), sample_rate_);
    }
    return true;
}

void LayerWriter::stopAndFinalize() {
    if (isThreadRunning()) {
        signalThreadShouldExit();
        stopThread(kStopTimeout);
    }
    writer_.reset();  // closes file
}

void LayerWriter::writeSilence(std::uint32_t frames,
                                juce::AudioBuffer<float>& planar) {
    planar.clear(0, static_cast<int>(frames));
    writer_->writeFromAudioSampleBuffer(planar, 0, static_cast<int>(frames));
    if (thumbnail_ != nullptr) {
        const auto sample_offset =
            static_cast<juce::int64>(samples_written_.load(std::memory_order_relaxed));
        thumbnail_->addBlock(sample_offset, planar, 0, static_cast<int>(frames));
    }
    samples_written_.fetch_add(frames, std::memory_order_relaxed);
}

void LayerWriter::drainChunk(std::uint32_t frames,
                              std::vector<float>& interleaved,
                              juce::AudioBuffer<float>& planar) {
    SpscRingBuffer rb(region_->slots[slot_].ring_header,
                       region_->slots[slot_].ring_data,
                       RING_FRAMES, RING_CHANNELS);
    if (!rb.peekAt(read_pos_, interleaved.data(), frames)) {
        // Data has been overwritten by overrun. Resync to current wp.
        read_pos_ = rb.writePos();
        return;
    }
    auto* L = planar.getWritePointer(0);
    auto* R = planar.getWritePointer(1);
    for (std::uint32_t i = 0; i < frames; ++i) {
        L[i] = interleaved[static_cast<std::size_t>(i) * RING_CHANNELS + 0];
        R[i] = interleaved[static_cast<std::size_t>(i) * RING_CHANNELS + 1];
    }
    writer_->writeFromAudioSampleBuffer(planar, 0, static_cast<int>(frames));
    if (thumbnail_ != nullptr) {
        // Feed the same chunk to the live thumbnail. addBlock is safe to call
        // from this thread; the GUI thread paints via drawChannel.
        const auto sample_offset =
            static_cast<juce::int64>(samples_written_.load(std::memory_order_relaxed));
        thumbnail_->addBlock(sample_offset, planar, 0, static_cast<int>(frames));
    }
    read_pos_ += frames;
    samples_written_.fetch_add(frames, std::memory_order_relaxed);
}

void LayerWriter::run() {
    SpscRingBuffer rb(region_->slots[slot_].ring_header,
                       region_->slots[slot_].ring_data,
                       RING_FRAMES, RING_CHANNELS);

    juce::AudioBuffer<float> planar(static_cast<int>(RING_CHANNELS),
                                      static_cast<int>(kChunkFrames));
    std::vector<float>       interleaved(static_cast<std::size_t>(kChunkFrames)
                                            * RING_CHANNELS);

    while (!threadShouldExit()) {
        const auto wp        = rb.writePos();
        const auto real_avail = (wp > read_pos_) ? (wp - read_pos_) : 0ull;

        // Prefer real audio when we have at least a full chunk.
        if (real_avail >= kChunkFrames) {
            drainChunk(kChunkFrames, interleaved, planar);
            continue;
        }

        // Pad silence to catch up to the audio thread's expected count when
        // the toggle wired us an expected_samples_ pointer. Sat tracks that
        // some hosts gate on clip presence won't produce data continuously;
        // padding keeps the WAV's timeline locked to play-start.
        if (expected_samples_ != nullptr) {
            const auto expected = expected_samples_->load(std::memory_order_acquire);
            const auto written  = samples_written_.load(std::memory_order_relaxed);
            const auto behind   = (expected > written) ? (expected - written) : 0ull;

            if (behind > 0) {
                if (real_avail > 0) {
                    // Drain whatever real samples there are before padding.
                    const auto take = static_cast<std::uint32_t>(
                        std::min<std::uint64_t>(real_avail, behind));
                    drainChunk(take, interleaved, planar);
                } else {
                    const auto pad = static_cast<std::uint32_t>(
                        std::min<std::uint64_t>(behind, kChunkFrames));
                    writeSilence(pad, planar);
                }
                continue;
            }
        }

        wait(kWaitMs);
    }

    // Drain remaining samples up to current wp.
    while (true) {
        const auto wp    = rb.writePos();
        const auto avail = (wp > read_pos_) ? (wp - read_pos_) : 0ull;
        if (avail == 0) break;
        const auto take = static_cast<std::uint32_t>(
            std::min<std::uint64_t>(avail, kChunkFrames));
        drainChunk(take, interleaved, planar);
        if (rb.writePos() == read_pos_) break;  // peekAt failed, abort drain
    }
}

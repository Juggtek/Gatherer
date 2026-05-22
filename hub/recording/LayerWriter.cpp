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

    constexpr std::uint32_t kPadStep = 64;

    // Sat-activity tracking. If sat advanced its wp recently, treat any
    // momentary real_avail==0 as scheduling jitter (one block of callback
    // variance is normal) and wait for it instead of injecting silence.
    // Only pad once wp has been stuck long enough that the source clearly
    // is not producing — which is the only state where alignment-by-pad is
    // actually correct.
    //
    // Without this gate the writer would: drain the audio-onset block,
    // observe real_avail==0 in the very next iteration (sat hasn't fired
    // the *next* block yet), and write kPadStep samples of silence into
    // the file before sat catches up. With pad-on this repeated until the
    // visible 13–25 ms gap we see right at audio onset on every slot.
    constexpr int kPadStuckMs = 30;
    std::uint64_t last_wp_seen     = rb.writePos();
    auto          last_wp_advance  = juce::Time::getMillisecondCounter();

    while (!threadShouldExit()) {
        const auto wp        = rb.writePos();
        if (wp != last_wp_seen) {
            last_wp_seen    = wp;
            last_wp_advance = juce::Time::getMillisecondCounter();
        }
        const auto real_avail = (wp > read_pos_) ? (wp - read_pos_) : 0ull;

        if (expected_samples_ != nullptr) {
            // Pad-silence ON: samples_written strictly tracks expected — no
            // writes past it, no shortfall. Once expected freezes (DAW stops
            // playing), the writer naturally stops, so every armed slot's
            // WAV ends at the same session-relative sample count regardless
            // of whether the sat kept getting processBlock past DAW-stop.
            const auto expected = expected_samples_->load(std::memory_order_acquire);
            const auto written  = samples_written_.load(std::memory_order_relaxed);
            if (written >= expected) {
                wait(kWaitMs);
                continue;
            }
            const auto needed   = expected - written;
            const auto take_real = std::min<std::uint64_t>(real_avail, needed);

            if (take_real >= static_cast<std::uint64_t>(kChunkFrames)) {
                drainChunk(kChunkFrames, interleaved, planar);
            } else if (take_real > 0) {
                drainChunk(static_cast<std::uint32_t>(take_real), interleaved, planar);
            } else {
                const auto stuck_ms = juce::Time::getMillisecondCounter() - last_wp_advance;
                if (stuck_ms < static_cast<std::uint32_t>(kPadStuckMs)) {
                    // Sat was advancing recently — this is callback jitter,
                    // not a silent source. Wait briefly and re-check at the
                    // top of the loop instead of padding.
                    wait(2);
                } else {
                    // Sat has been stuck long enough that we're confident
                    // it's a real silent period (e.g. clip-gated track).
                    // Pad to keep samples_written aligned to expected.
                    const auto pad = static_cast<std::uint32_t>(
                        std::min<std::uint64_t>(needed,
                                                 static_cast<std::uint64_t>(kPadStep)));
                    writeSilence(pad, planar);
                }
            }
            continue;
        }

        // Pad-silence OFF: original behaviour — drain whenever a chunk's
        // worth is available, otherwise wait.
        if (real_avail >= kChunkFrames) {
            drainChunk(kChunkFrames, interleaved, planar);
        } else {
            wait(kWaitMs);
        }
    }

    // Determine the target length once — `expected_samples` is frozen now that
    // recording_active has gone false and the audio thread no longer increments it.
    const std::uint64_t target =
        (expected_samples_ != nullptr)
            ? expected_samples_->load(std::memory_order_acquire)
            : std::numeric_limits<std::uint64_t>::max();

    // Drain remaining real samples up to the target (or until sat stops).
    while (true) {
        const auto written = samples_written_.load(std::memory_order_relaxed);
        if (written >= target) break;
        const auto wp    = rb.writePos();
        const auto avail = (wp > read_pos_) ? (wp - read_pos_) : 0ull;
        if (avail == 0) break;
        const auto take = static_cast<std::uint32_t>(
            std::min<std::uint64_t>({avail, target - written,
                                     static_cast<std::uint64_t>(kChunkFrames)}));
        drainChunk(take, interleaved, planar);
        if (rb.writePos() == read_pos_) break;  // peekAt failed, abort drain
    }

    // Pad silence to the target so every armed slot's WAV ends at the same
    // session-relative sample position. Without this, slots whose sat stopped
    // writing before stopRecording end up shorter than slots that kept going.
    if (expected_samples_ != nullptr) {
        while (true) {
            const auto written = samples_written_.load(std::memory_order_relaxed);
            if (written >= target) break;
            const auto pad = static_cast<std::uint32_t>(
                std::min<std::uint64_t>(target - written,
                                         static_cast<std::uint64_t>(kChunkFrames)));
            writeSilence(pad, planar);
        }
    }
}

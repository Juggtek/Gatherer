#include "PlaybackEngine.h"

namespace gatherer::playback {

Engine::Engine() {
    fmt_manager_.registerBasicFormats();
}

Engine::~Engine() = default;

void Engine::prepare(double host_sample_rate, int max_block_size) {
    sample_rate_    = host_sample_rate;
    max_block_size_ = max_block_size;
    scratch_planar_.setSize(2, std::max(max_block_size, 1), false, true, true);
}

void Engine::release() {
    scratch_planar_.setSize(0, 0);
}

void Engine::setSourceForSlot(int slot, const juce::File& wav) {
    if (slot < 0 || slot >= static_cast<int>(sources_.size())) return;

    // The audio thread reads via readSlotIntoInterleaved which only runs when
    // state == Playing. We force-stop transport here so the swap is race-free.
    stop();

    Source& src = sources_[slot];
    src.reader.reset();
    src.length         = 0;
    src.offset_samples = 0;

    if (wav == juce::File{} || !wav.existsAsFile()) {
        recomputeSessionLength();
        return;
    }

    std::unique_ptr<juce::AudioFormatReader> reader{ fmt_manager_.createReaderFor(wav) };
    if (!reader) {
        recomputeSessionLength();
        return;
    }
    // For now we only handle reader sample rates that match the host. If they
    // differ we'd need to resample on the audio thread — out of scope for v1.
    if (sample_rate_ > 0.0 && std::abs(reader->sampleRate - sample_rate_) > 0.5) {
        reader.reset();
        recomputeSessionLength();
        return;
    }
    src.length = static_cast<std::int64_t>(reader->lengthInSamples);
    src.reader = std::move(reader);
    recomputeSessionLength();
}

void Engine::clearAll() {
    stop();
    for (auto& s : sources_) {
        s.reader.reset();
        s.length         = 0;
        s.offset_samples = 0;
    }
    session_length_.store(0, std::memory_order_release);
}

bool Engine::hasSourceForSlot(int slot) const noexcept {
    if (slot < 0 || slot >= static_cast<int>(sources_.size())) return false;
    return sources_[slot].reader != nullptr;
}

std::int64_t Engine::slotLengthSamples(int slot) const noexcept {
    if (slot < 0 || slot >= static_cast<int>(sources_.size())) return 0;
    return sources_[slot].length;
}

void Engine::setSlotOffsetSamples(int slot, std::int64_t offset) {
    if (slot < 0 || slot >= static_cast<int>(sources_.size())) return;
    if (offset < 0) offset = 0;
    sources_[slot].offset_samples = offset;
    recomputeSessionLength();
}

std::int64_t Engine::slotOffsetSamples(int slot) const noexcept {
    if (slot < 0 || slot >= static_cast<int>(sources_.size())) return 0;
    return sources_[slot].offset_samples;
}

std::int64_t Engine::sessionLengthSamples() const noexcept {
    return session_length_.load(std::memory_order_acquire);
}

double Engine::sessionLengthSeconds() const noexcept {
    return sample_rate_ > 0.0
        ? static_cast<double>(sessionLengthSamples()) / sample_rate_
        : 0.0;
}

void Engine::play() {
    if (sessionLengthSamples() <= 0) return;
    if (playhead_.load(std::memory_order_acquire) >= sessionLengthSamples()) {
        playhead_.store(0, std::memory_order_release);
    }
    state_.store(static_cast<int>(State::Playing), std::memory_order_release);
}

void Engine::pause() {
    if (state() == State::Playing) {
        state_.store(static_cast<int>(State::Paused), std::memory_order_release);
    }
}

void Engine::stop() {
    state_.store(static_cast<int>(State::Stopped), std::memory_order_release);
    playhead_.store(0, std::memory_order_release);
}

void Engine::seekSamples(std::int64_t pos) {
    pos = juce::jlimit<std::int64_t>(0, sessionLengthSamples(), pos);
    playhead_.store(pos, std::memory_order_release);
}

void Engine::seekSeconds(double s) {
    if (sample_rate_ <= 0.0) return;
    seekSamples(static_cast<std::int64_t>(s * sample_rate_));
}

double Engine::playheadSeconds() const noexcept {
    return sample_rate_ > 0.0
        ? static_cast<double>(playheadSamples()) / sample_rate_
        : 0.0;
}

bool Engine::readSlotIntoInterleaved(int slot, float* dest, int frames) {
    if (slot < 0 || slot >= static_cast<int>(sources_.size())) return false;
    if (state() != State::Playing) return false;
    auto& src = sources_[slot];
    if (!src.reader) return false;

    const auto pos       = playhead_.load(std::memory_order_acquire);
    const auto pos_local = pos - src.offset_samples;

    // Outside this slot's recorded range — silence. Return true so the caller
    // doesn't fall back to live sat audio for slots that simply haven't started
    // yet (or have already ended) in the session playback.
    if (pos_local < 0 || pos_local >= src.length) {
        std::memset(dest, 0, sizeof(float) * static_cast<std::size_t>(frames) * 2);
        return true;
    }

    const auto available = static_cast<int>(std::min<std::int64_t>(src.length - pos_local, frames));
    if (available <= 0) return false;

    // Read planar into our scratch, then interleave.
    if (scratch_planar_.getNumSamples() < frames)
        scratch_planar_.setSize(2, frames, false, true, true);
    scratch_planar_.clear(0, frames);

    juce::AudioBuffer<float>* buf = &scratch_planar_;
    const bool useL = src.reader->numChannels >= 1;
    const bool useR = src.reader->numChannels >= 2;
    src.reader->read(buf, 0, available, pos_local, useL, useR);
    // Mono → duplicate to R.
    if (!useR) {
        scratch_planar_.copyFrom(1, 0, scratch_planar_, 0, 0, available);
    }

    const float* L = scratch_planar_.getReadPointer(0);
    const float* R = scratch_planar_.getReadPointer(1);
    for (int i = 0; i < available; ++i) {
        dest[i * 2 + 0] = L[i];
        dest[i * 2 + 1] = R[i];
    }
    // Zero-fill any tail past the source's end.
    for (int i = available; i < frames; ++i) {
        dest[i * 2 + 0] = 0.0f;
        dest[i * 2 + 1] = 0.0f;
    }
    return true;
}

void Engine::advancePlayhead(int frames) {
    if (state() != State::Playing) return;
    const auto next = playhead_.load(std::memory_order_acquire) + frames;
    const auto end  = sessionLengthSamples();
    if (next >= end) {
        // Auto-stop at end of session; keep playhead pinned at end so the UI
        // shows the final position. Caller can press Play again to restart.
        playhead_.store(end, std::memory_order_release);
        state_.store(static_cast<int>(State::Paused), std::memory_order_release);
    } else {
        playhead_.store(next, std::memory_order_release);
    }
}

void Engine::recomputeSessionLength() {
    std::int64_t m = 0;
    for (const auto& s : sources_) {
        if (s.length > 0) {
            m = std::max(m, s.offset_samples + s.length);
        }
    }
    session_length_.store(m, std::memory_order_release);
    if (playhead_.load(std::memory_order_relaxed) > m) {
        playhead_.store(m, std::memory_order_release);
    }
}

}  // namespace gatherer::playback

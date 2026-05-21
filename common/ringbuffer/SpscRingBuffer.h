#pragma once

#include <algorithm>
#include <atomic>
#include <cstdint>
#include <cstring>

namespace gatherer {

// Lock-free single-producer single-consumer ring buffer for interleaved float frames.
//
// Storage is provided externally — typically embedded in a shared memory region. The class
// is a non-owning view that operates on the caller's `Header` and `data` array.
//
// Conventions:
//   - `capacity_frames` must be a power of two (mask = capacity - 1).
//   - `data` is laid out as interleaved frames: data[frame * channels + ch].
//   - `write_pos` / `read_pos` are monotonic 64-bit counters (no wrap-around concerns
//      at any practical audio rate over any practical session length).
//   - Producer-only fields: `write_pos`.
//   - Consumer-only fields: `read_pos`.
//   - Overrun policy: if the producer would exceed capacity, it advances `read_pos`
//     to drop the oldest data. The producer never blocks; the consumer sees a jump
//     in `read_pos` and can detect this externally.
class SpscRingBuffer {
public:
    struct Header {
        std::atomic<std::uint64_t> write_pos;
        std::atomic<std::uint64_t> read_pos;
    };

    static_assert(std::atomic<std::uint64_t>::is_always_lock_free,
                  "uint64 atomic must be lock-free for shm SPSC");

    SpscRingBuffer(Header& header,
                   float* data,
                   std::uint32_t capacity_frames,
                   std::uint32_t channels) noexcept
        : header_(&header),
          data_(data),
          capacity_(capacity_frames),
          mask_(capacity_frames - 1u),
          channels_(channels)
    {
        // capacity must be power of two for the mask trick.
        // (Validate in debug; in release rely on caller.)
    }

    // One-time initialization of an empty ring (call once, from the creator side).
    static void initialize(Header& header) noexcept {
        header.write_pos.store(0, std::memory_order_relaxed);
        header.read_pos.store(0,  std::memory_order_relaxed);
    }

    std::uint32_t capacityFrames() const noexcept { return capacity_; }
    std::uint32_t channels()       const noexcept { return channels_; }

    std::uint64_t writePos() const noexcept {
        return header_->write_pos.load(std::memory_order_acquire);
    }
    std::uint64_t readPos() const noexcept {
        return header_->read_pos.load(std::memory_order_acquire);
    }
    std::uint32_t availableToRead() const noexcept {
        return static_cast<std::uint32_t>(writePos() - readPos());
    }
    std::uint32_t spaceToWrite() const noexcept {
        return capacity_ - availableToRead();
    }

    // Producer. Writes `frames` interleaved frames. If insufficient space, advances
    // read_pos to drop the oldest data (overrun policy). Returns the new write_pos.
    std::uint64_t write(const float* src, std::uint32_t frames) noexcept {
        if (frames > capacity_) {
            // Drop the leading samples that can't possibly fit.
            src    += static_cast<std::size_t>(frames - capacity_) * channels_;
            frames  = capacity_;
        }

        const auto w = header_->write_pos.load(std::memory_order_relaxed);
        const auto r = header_->read_pos.load(std::memory_order_acquire);
        const auto avail = static_cast<std::uint32_t>(w - r);
        const auto space = capacity_ - avail;

        if (frames > space) {
            const auto drop = frames - space;
            header_->read_pos.store(r + drop, std::memory_order_release);
        }

        const auto start      = static_cast<std::uint32_t>(w & mask_);
        const auto firstChunk = std::min(frames, capacity_ - start);

        std::memcpy(&data_[static_cast<std::size_t>(start) * channels_],
                    src,
                    static_cast<std::size_t>(firstChunk) * channels_ * sizeof(float));

        if (frames > firstChunk) {
            std::memcpy(&data_[0],
                        src + static_cast<std::size_t>(firstChunk) * channels_,
                        static_cast<std::size_t>(frames - firstChunk) * channels_ * sizeof(float));
        }

        const auto new_w = w + frames;
        header_->write_pos.store(new_w, std::memory_order_release);
        return new_w;
    }

    // Consumer. Reads up to `frames` interleaved frames into `dst`. Returns frames read.
    std::uint32_t read(float* dst, std::uint32_t frames) noexcept {
        const auto r = header_->read_pos.load(std::memory_order_relaxed);
        const auto w = header_->write_pos.load(std::memory_order_acquire);
        const auto avail  = static_cast<std::uint32_t>(w - r);
        const auto toRead = std::min(frames, avail);
        if (toRead == 0) return 0;

        const auto start      = static_cast<std::uint32_t>(r & mask_);
        const auto firstChunk = std::min(toRead, capacity_ - start);

        std::memcpy(dst,
                    &data_[static_cast<std::size_t>(start) * channels_],
                    static_cast<std::size_t>(firstChunk) * channels_ * sizeof(float));

        if (toRead > firstChunk) {
            std::memcpy(dst + static_cast<std::size_t>(firstChunk) * channels_,
                        &data_[0],
                        static_cast<std::size_t>(toRead - firstChunk) * channels_ * sizeof(float));
        }

        header_->read_pos.store(r + toRead, std::memory_order_release);
        return toRead;
    }

    // Consumer-side resync: jump read_pos forward to current write_pos. Use after
    // detecting an overrun if the consumer wants to discard everything and resume fresh.
    void resyncToWrite() noexcept {
        const auto w = header_->write_pos.load(std::memory_order_acquire);
        header_->read_pos.store(w, std::memory_order_release);
    }

    // Consumer-side explicit positioning. Set read_pos to an arbitrary value, e.g. to
    // establish a target lag (write_pos - target_lag) at hub init. Caller is responsible
    // for the value being in the valid window [write_pos - capacity, write_pos].
    void setReadPos(std::uint64_t pos) noexcept {
        header_->read_pos.store(pos, std::memory_order_release);
    }

    // Random-access read at a specific monotonic ring position, without touching read_pos.
    // Returns true if `frames` frames starting at `position` were fully readable, false if
    // the requested range is either not yet written or already overwritten by the producer's
    // overrun policy. On false return, `dst` is left untouched.
    //
    // Intended for the hub's playhead-indexed read pattern: caller computes the desired
    // monotonic position from a host playhead and the slot's last_write_host_frame.
    bool peekAt(std::uint64_t position, float* dst, std::uint32_t frames) const noexcept {
        const auto wp = header_->write_pos.load(std::memory_order_acquire);
        if (position + frames > wp) return false;          // tail not yet written
        if (wp - position > capacity_) return false;       // head already overwritten

        const auto start      = static_cast<std::uint32_t>(position & mask_);
        const auto firstChunk = std::min(frames, capacity_ - start);

        std::memcpy(dst,
                    &data_[static_cast<std::size_t>(start) * channels_],
                    static_cast<std::size_t>(firstChunk) * channels_ * sizeof(float));

        if (frames > firstChunk) {
            std::memcpy(dst + static_cast<std::size_t>(firstChunk) * channels_,
                        &data_[0],
                        static_cast<std::size_t>(frames - firstChunk) * channels_ * sizeof(float));
        }
        return true;
    }

private:
    Header*       header_;
    float*        data_;
    std::uint32_t capacity_;
    std::uint32_t mask_;
    std::uint32_t channels_;
};

} // namespace gatherer

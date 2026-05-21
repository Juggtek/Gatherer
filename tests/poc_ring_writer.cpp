#include "poc_ring_shared.h"
#include "ringbuffer/SpscRingBuffer.h"
#include "shm/SharedMemory.h"

#include <atomic>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <new>
#include <thread>

using namespace gatherer;
using namespace gatherer::poc;

namespace {
std::atomic<bool> g_stop{false};
void onSignal(int) { g_stop.store(true); }

// Sample values encode the global frame index so the reader can verify continuity
// bit-exactly. Float has 24 mantissa bits → uniquely represents integers up to 2^24.
constexpr std::uint64_t SAMPLE_MOD = 1ull << 24;
}

int main() {
    std::signal(SIGINT,  onSignal);
    std::signal(SIGTERM, onSignal);

    SharedMemory::unlink(RING_SHM_NAME);

    try {
        SharedMemory shm(RING_SHM_NAME, sizeof(RingLayout), SharedMemory::Mode::CreateNew);
        std::fprintf(stdout, "[ring-writer] shm '%s' size=%zu owner=%d\n",
                     RING_SHM_NAME, shm.size(), shm.isOwner());

        auto* layout = new (shm.data()) RingLayout{};
        layout->magic.store(RING_MAGIC, std::memory_order_relaxed);
        SpscRingBuffer::initialize(layout->rb_header);
        layout->ready.store(1, std::memory_order_release);

        SpscRingBuffer rb(layout->rb_header, layout->data, RING_CAPACITY, RING_CHANNELS);
        std::fprintf(stdout, "[ring-writer] ring init: capacity=%u channels=%u\n",
                     rb.capacityFrames(), rb.channels());

        constexpr std::uint32_t BLOCK = 64;
        float buf[BLOCK * RING_CHANNELS];

        std::uint64_t idx = 0;
        std::uint64_t blocks_written = 0;
        std::uint64_t overruns_caused = 0;
        auto last_log = std::chrono::steady_clock::now();

        while (!g_stop.load(std::memory_order_relaxed)) {
            for (std::uint32_t k = 0; k < BLOCK; ++k) {
                const float v = static_cast<float>(idx & (SAMPLE_MOD - 1));
                buf[k * RING_CHANNELS + 0] = v;
                buf[k * RING_CHANNELS + 1] = v;
                ++idx;
            }

            const auto pre_read = rb.readPos();
            rb.write(buf, BLOCK);
            const auto post_read = rb.readPos();
            if (post_read != pre_read) ++overruns_caused;
            ++blocks_written;

            // Pace approximately like a 48 kHz / 64-frame audio thread: ~1.33 ms / block.
            std::this_thread::sleep_for(std::chrono::microseconds(1333));

            const auto now = std::chrono::steady_clock::now();
            if (now - last_log >= std::chrono::seconds(1)) {
                std::fprintf(stdout,
                    "[ring-writer] frames=%llu blocks=%llu writePos=%llu readPos=%llu "
                    "avail=%u overruns=%llu\n",
                    static_cast<unsigned long long>(idx),
                    static_cast<unsigned long long>(blocks_written),
                    static_cast<unsigned long long>(rb.writePos()),
                    static_cast<unsigned long long>(rb.readPos()),
                    rb.availableToRead(),
                    static_cast<unsigned long long>(overruns_caused));
                std::fflush(stdout);
                last_log = now;
            }
        }

        std::fprintf(stdout, "[ring-writer] exit. total_frames=%llu overruns=%llu\n",
                     static_cast<unsigned long long>(idx),
                     static_cast<unsigned long long>(overruns_caused));
        return 0;
    } catch (const std::exception& e) {
        std::fprintf(stderr, "[ring-writer] error: %s\n", e.what());
        return 1;
    }
}

#include "poc_ring_shared.h"
#include "ringbuffer/SpscRingBuffer.h"
#include "shm/SharedMemory.h"

#include <atomic>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <thread>

using namespace gatherer;
using namespace gatherer::poc;

namespace {
std::atomic<bool> g_stop{false};
void onSignal(int) { g_stop.store(true); }

constexpr std::uint64_t SAMPLE_MOD = 1ull << 24;
}

int main() {
    std::signal(SIGINT,  onSignal);
    std::signal(SIGTERM, onSignal);

    try {
        SharedMemory shm(RING_SHM_NAME, sizeof(RingLayout), SharedMemory::Mode::OpenExisting);
        std::fprintf(stdout, "[ring-reader] attached size=%zu\n", shm.size());

        auto* layout = static_cast<RingLayout*>(shm.data());

        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
        while (layout->ready.load(std::memory_order_acquire) == 0) {
            if (std::chrono::steady_clock::now() > deadline) {
                std::fprintf(stderr, "[ring-reader] timeout waiting for writer ready\n");
                return 2;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
        if (layout->magic.load(std::memory_order_acquire) != RING_MAGIC) {
            std::fprintf(stderr, "[ring-reader] bad magic\n");
            return 3;
        }

        SpscRingBuffer rb(layout->rb_header, layout->data, RING_CAPACITY, RING_CHANNELS);

        constexpr std::uint32_t BLOCK = 64;
        float buf[BLOCK * RING_CHANNELS];

        std::uint64_t expected = 0;       // global frame index we expect next
        std::uint64_t frames_read = 0;
        std::uint64_t gaps = 0;
        std::uint64_t mismatches = 0;
        bool primed = false;
        auto last_log = std::chrono::steady_clock::now();

        while (!g_stop.load(std::memory_order_relaxed)) {
            const auto got = rb.read(buf, BLOCK);
            if (got == 0) {
                std::this_thread::sleep_for(std::chrono::microseconds(500));
                continue;
            }

            for (std::uint32_t k = 0; k < got; ++k) {
                const auto v0 = static_cast<std::uint64_t>(buf[k * RING_CHANNELS + 0]);
                const auto v1 = static_cast<std::uint64_t>(buf[k * RING_CHANNELS + 1]);
                if (v0 != v1) {
                    ++mismatches;
                }

                if (!primed) {
                    expected = v0;
                    primed = true;
                } else if (v0 != (expected & (SAMPLE_MOD - 1))) {
                    ++gaps;
                    // Resync expectation to current observed value.
                    expected = v0;
                }
                ++expected;
            }
            frames_read += got;

            const auto now = std::chrono::steady_clock::now();
            if (now - last_log >= std::chrono::seconds(1)) {
                std::fprintf(stdout,
                    "[ring-reader] frames=%llu gaps=%llu mismatches=%llu "
                    "writePos=%llu readPos=%llu avail=%u\n",
                    static_cast<unsigned long long>(frames_read),
                    static_cast<unsigned long long>(gaps),
                    static_cast<unsigned long long>(mismatches),
                    static_cast<unsigned long long>(rb.writePos()),
                    static_cast<unsigned long long>(rb.readPos()),
                    rb.availableToRead());
                std::fflush(stdout);
                last_log = now;
            }
        }

        std::fprintf(stdout,
            "[ring-reader] exit. total_frames=%llu gaps=%llu mismatches=%llu\n",
            static_cast<unsigned long long>(frames_read),
            static_cast<unsigned long long>(gaps),
            static_cast<unsigned long long>(mismatches));
        // Mismatches between L/R channels indicate a torn read across channels:
        // a real correctness failure. Gaps are expected if reader fell behind
        // (overrun policy by design).
        return mismatches == 0 ? 0 : 4;
    } catch (const std::exception& e) {
        std::fprintf(stderr, "[ring-reader] error: %s\n", e.what());
        return 1;
    }
}

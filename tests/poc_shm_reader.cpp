#include "poc_shared.h"
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
}

int main() {
    std::signal(SIGINT,  onSignal);
    std::signal(SIGTERM, onSignal);

    try {
        SharedMemory shm(SHM_NAME, sizeof(PocLayout), SharedMemory::Mode::OpenExisting);
        std::fprintf(stdout, "[reader] attached to shm '%s' size=%zu owner=%d\n",
                     SHM_NAME, shm.size(), shm.isOwner());

        auto* layout = static_cast<PocLayout*>(shm.data());

        // Wait for the writer to mark ready.
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
        while (layout->ready.load(std::memory_order_acquire) == 0) {
            if (std::chrono::steady_clock::now() > deadline) {
                std::fprintf(stderr, "[reader] timed out waiting for writer ready\n");
                return 2;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }

        const auto magic = layout->magic.load(std::memory_order_acquire);
        if (magic != MAGIC) {
            std::fprintf(stderr, "[reader] bad magic: got 0x%08x expected 0x%08x\n",
                         magic, MAGIC);
            return 3;
        }
        std::fprintf(stdout, "[reader] magic ok; sampling counter. Ctrl-C to stop.\n");

        std::uint64_t prev = 0;
        std::uint64_t samples = 0;
        std::uint64_t bad = 0;
        auto last_log = std::chrono::steady_clock::now();

        while (!g_stop.load(std::memory_order_relaxed)) {
            const auto cur = layout->counter.load(std::memory_order_acquire);
            if (cur < prev) {
                ++bad;
                std::fprintf(stderr, "[reader] regression: %llu -> %llu\n",
                             static_cast<unsigned long long>(prev),
                             static_cast<unsigned long long>(cur));
            }
            prev = cur;
            ++samples;

            std::this_thread::sleep_for(std::chrono::milliseconds(100));

            const auto now = std::chrono::steady_clock::now();
            if (now - last_log >= std::chrono::seconds(1)) {
                std::fprintf(stdout, "[reader] counter = %llu samples = %llu bad = %llu\n",
                             static_cast<unsigned long long>(cur),
                             static_cast<unsigned long long>(samples),
                             static_cast<unsigned long long>(bad));
                std::fflush(stdout);
                last_log = now;
            }
        }

        std::fprintf(stdout, "[reader] done. samples=%llu regressions=%llu final=%llu\n",
                     static_cast<unsigned long long>(samples),
                     static_cast<unsigned long long>(bad),
                     static_cast<unsigned long long>(prev));
        return bad == 0 ? 0 : 4;
    } catch (const std::exception& e) {
        std::fprintf(stderr, "[reader] error: %s\n", e.what());
        return 1;
    }
}

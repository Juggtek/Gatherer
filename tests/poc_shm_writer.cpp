#include "poc_shared.h"
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
}

int main() {
    std::signal(SIGINT,  onSignal);
    std::signal(SIGTERM, onSignal);

    // Clean up any stale region from a previous crashed run so we always start fresh.
    SharedMemory::unlink(SHM_NAME);

    try {
        SharedMemory shm(SHM_NAME, sizeof(PocLayout), SharedMemory::Mode::CreateNew);
        std::fprintf(stdout, "[writer] created shm '%s' size=%zu owner=%d\n",
                     SHM_NAME, shm.size(), shm.isOwner());

        // Placement-new the layout in-place to initialize atomics correctly.
        auto* layout = new (shm.data()) PocLayout{};
        layout->magic.store(MAGIC, std::memory_order_relaxed);
        layout->counter.store(0, std::memory_order_relaxed);
        layout->last_value.store(0, std::memory_order_relaxed);
        // Publish ready *after* the other fields are visible.
        layout->ready.store(1, std::memory_order_release);

        std::fprintf(stdout, "[writer] initialized; writing counter at ~1 kHz. Ctrl-C to stop.\n");

        auto last_log = std::chrono::steady_clock::now();
        std::uint64_t n = 0;
        while (!g_stop.load(std::memory_order_relaxed)) {
            ++n;
            layout->counter.store(n, std::memory_order_release);
            layout->last_value.store(n, std::memory_order_release);

            std::this_thread::sleep_for(std::chrono::milliseconds(1));

            const auto now = std::chrono::steady_clock::now();
            if (now - last_log >= std::chrono::seconds(1)) {
                std::fprintf(stdout, "[writer] counter = %llu\n",
                             static_cast<unsigned long long>(n));
                std::fflush(stdout);
                last_log = now;
            }
        }

        std::fprintf(stdout, "[writer] exiting at counter = %llu\n",
                     static_cast<unsigned long long>(n));
        return 0;
    } catch (const std::exception& e) {
        std::fprintf(stderr, "[writer] error: %s\n", e.what());
        return 1;
    }
}

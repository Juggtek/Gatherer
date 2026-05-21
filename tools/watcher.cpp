// Read-only watcher for the Gatherer shared memory region. Attaches via OpenExisting
// (never creates anything) and never writes — safe to run alongside live plugins.

#include "protocol/SharedRegion.h"
#include "ringbuffer/SpscRingBuffer.h"
#include "shm/SharedMemory.h"

#include <atomic>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <cstring>
#include <string>
#include <thread>

using namespace gatherer;
using namespace gatherer::protocol;

namespace {
std::atomic<bool> g_stop{false};
void onSignal(int) { g_stop.store(true); }

// ANSI: clear screen + move cursor to home.
constexpr const char* CLR_HOME = "\x1b[2J\x1b[H";

std::string hex64(std::uint64_t v) {
    char buf[19];
    std::snprintf(buf, sizeof(buf), "0x%016llx", static_cast<unsigned long long>(v));
    return buf;
}

const char* stateName(std::uint32_t s) {
    switch (s) {
        case SLOT_STATE_EMPTY:   return "EMPTY";
        case SLOT_STATE_CLAIMED: return "CLAIMED";
        case SLOT_STATE_ACTIVE:  return "ACTIVE";
        default:                 return "?";
    }
}

void render(const SharedRegion& r) {
    std::printf("%s", CLR_HOME);
    std::printf("=== Gatherer Shared Memory Watcher ===\n");
    std::printf("shm '%s' (size %llu bytes)\n\n",
                SHM_NAME,
                static_cast<unsigned long long>(r.header.shm_size_bytes));

    const auto magic = r.header.magic;
    const auto version = r.header.version;
    const auto init_done = r.header.init_done.load(std::memory_order_acquire);
    const auto refcount = r.header.instance_refcount.load(std::memory_order_relaxed);
    const auto sr = r.header.sample_rate.load(std::memory_order_relaxed);
    const auto bs = r.header.max_block_size.load(std::memory_order_relaxed);
    const auto hub_uuid = r.header.hub_uuid.load(std::memory_order_acquire);
    const auto hub_pid = r.header.hub_pid.load(std::memory_order_relaxed);
    const auto hub_hb = r.header.hub_heartbeat.load(std::memory_order_relaxed);

    std::printf("header.magic          : 0x%08x %s\n", magic,
                magic == MAGIC ? "(GTHR ok)" : "(BAD MAGIC)");
    std::printf("header.version       : %u\n", version);
    std::printf("header.init_done     : %u\n", init_done);
    std::printf("header.num_slots     : %u\n", r.header.num_slots);
    std::printf("header.channels_per_slot: %u\n", r.header.channels_per_slot);
    std::printf("header.sample_rate   : %u Hz\n", sr);
    std::printf("header.max_block_size: %u\n", bs);
    std::printf("header.refcount      : %u\n", refcount);
    std::printf("header.hub_uuid      : %s  pid=%llu  hb=%llu\n",
                hex64(hub_uuid).c_str(),
                static_cast<unsigned long long>(hub_pid),
                static_cast<unsigned long long>(hub_hb));
    std::printf("\n");

    std::printf("%-4s  %-7s  %-12s  %-12s  %-8s  %-10s  %-12s  %-18s\n",
                "slot", "state", "writePos", "readPos", "lag", "hb", "uuid", "track");
    std::printf("%-4s  %-7s  %-12s  %-12s  %-8s  %-10s  %-12s  %-18s\n",
                "----", "-------", "------------", "------------", "--------",
                "----------", "------------", "------------------");

    int active_count = 0;
    for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
        const auto& slot = r.slots[i];
        const auto state = slot.state.load(std::memory_order_acquire);
        if (state == SLOT_STATE_EMPTY) continue;
        if (state == SLOT_STATE_ACTIVE) ++active_count;

        const auto wp     = slot.ring_header.write_pos.load(std::memory_order_acquire);
        const auto rp     = slot.ring_header.read_pos.load(std::memory_order_acquire);
        const auto lag    = wp - rp;
        const auto hb     = slot.sat_heartbeat.load(std::memory_order_relaxed);
        const auto uuid   = slot.sat_uuid.load(std::memory_order_acquire);

        char track[20] = {0};
        std::strncpy(track, slot.track_name, sizeof(track) - 1);

        // Show last 4 hex chars of uuid for brevity.
        char uuid_short[6] = {0};
        std::snprintf(uuid_short, sizeof(uuid_short), "%04llx",
                      static_cast<unsigned long long>(uuid & 0xFFFFull));

        std::printf("%-4u  %-7s  %-12llu  %-12llu  %-8llu  %-10llu  ..%-10s  %-18s\n",
                    i,
                    stateName(state),
                    static_cast<unsigned long long>(wp),
                    static_cast<unsigned long long>(rp),
                    static_cast<unsigned long long>(lag),
                    static_cast<unsigned long long>(hb),
                    uuid_short,
                    track);
    }
    std::printf("\nactive satellites: %d\n", active_count);
    std::printf("(Ctrl-C to exit)\n");
    std::fflush(stdout);
}

}  // namespace

int main() {
    std::signal(SIGINT,  onSignal);
    std::signal(SIGTERM, onSignal);

    try {
        SharedMemory shm(SHM_NAME, sizeof(SharedRegion), SharedMemory::Mode::OpenExisting);
        const auto* region = static_cast<const SharedRegion*>(shm.data());

        while (!g_stop.load(std::memory_order_relaxed)) {
            render(*region);
            std::this_thread::sleep_for(std::chrono::milliseconds(100));
        }
        std::printf("\nwatcher exit.\n");
        return 0;
    } catch (const std::exception& e) {
        std::fprintf(stderr, "watcher error: %s\n"
                             "(is anything attached to '%s'? Load Gatherer Sat or Hub first.)\n",
                     e.what(), SHM_NAME);
        return 1;
    }
}

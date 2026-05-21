#pragma once

#include <atomic>
#include <cstdint>
#include <cstring>
#include <random>

#include "protocol/SharedRegion.h"

namespace gatherer::protocol {

// Generate a non-zero 64-bit identifier. Used as a per-instance UUID for both
// satellites and the hub. Stable enough for the PoC; can be swapped for RFC 4122
// later without changing the protocol.
inline std::uint64_t generateInstanceId() noexcept {
    thread_local std::mt19937_64 gen{std::random_device{}()};
    std::uint64_t v = 0;
    while (v == 0) v = gen();
    return v;
}

// Returns slot index (0..NUM_SLOTS-1) on success, or -1 if no slot was free.
//
// Reclaim policy:
//   1. If a slot already holds `my_uuid`, reuse it (e.g. on prepareToPlay after a
//      project reload that restored state).
//   2. Otherwise CAS an EMPTY slot to CLAIMED, fill identity, init the ring buffer,
//      then publish ACTIVE.
//
// Stale-slot reclaim (dead PID, expired heartbeat) is intentionally not implemented
// in this stub — see DESIGN.md §11, item 3.
inline int claimSlot(SharedRegion& region,
                     std::uint64_t my_uuid,
                     std::uint64_t my_pid,
                     const char* display_name,
                     const char* track_name,
                     std::uint32_t color_rgba) noexcept
{
    // Pass 1: existing slot for our UUID.
    for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
        auto& slot = region.slots[i];
        if (slot.sat_uuid.load(std::memory_order_acquire) == my_uuid
            && slot.state.load(std::memory_order_acquire) != SLOT_STATE_EMPTY) {
            slot.sat_pid.store(my_pid, std::memory_order_release);
            slot.state.store(SLOT_STATE_ACTIVE, std::memory_order_release);
            return static_cast<int>(i);
        }
    }

    // Pass 2: claim a free slot.
    for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
        auto& slot = region.slots[i];
        std::uint32_t expected = SLOT_STATE_EMPTY;
        if (slot.state.compare_exchange_strong(expected, SLOT_STATE_CLAIMED,
                                               std::memory_order_acq_rel)) {
            slot.sat_uuid.store(my_uuid, std::memory_order_release);
            slot.sat_pid.store(my_pid, std::memory_order_release);
            slot.sat_heartbeat.store(0, std::memory_order_release);
            slot.anchor_host_frame.store(0, std::memory_order_release);
            slot.last_write_host_frame.store(0, std::memory_order_release);
            slot.cal_session_acked.store(0, std::memory_order_release);
            slot.cal_start_hub_heartbeat.store(0, std::memory_order_release);
            slot.cal_start_wp.store(0, std::memory_order_release);
            slot.color_rgba = color_rgba;

            std::memset(slot.display_name, 0, sizeof(slot.display_name));
            std::memset(slot.track_name,   0, sizeof(slot.track_name));
            if (display_name) {
                std::strncpy(slot.display_name, display_name, sizeof(slot.display_name) - 1);
            }
            if (track_name) {
                std::strncpy(slot.track_name, track_name, sizeof(slot.track_name) - 1);
            }

            SpscRingBuffer::initialize(slot.ring_header);

            slot.state.store(SLOT_STATE_ACTIVE, std::memory_order_release);
            return static_cast<int>(i);
        }
    }

    return -1;
}

// Mark a slot empty. Caller must own the slot (matching UUID). Ring data is
// intentionally left intact for the next reclaim.
inline void releaseSlot(SharedRegion& region, int slot_index, std::uint64_t my_uuid) noexcept {
    if (slot_index < 0 || slot_index >= static_cast<int>(NUM_SLOTS)) return;
    auto& slot = region.slots[slot_index];
    if (slot.sat_uuid.load(std::memory_order_acquire) != my_uuid) return;
    slot.sat_uuid.store(0, std::memory_order_release);
    slot.sat_pid.store(0, std::memory_order_release);
    slot.state.store(SLOT_STATE_EMPTY, std::memory_order_release);
}

// Try to become the hub. Returns true on success. Refuses if a hub is already
// registered (stub PoC: no liveness check on existing hub).
inline bool claimHub(SharedRegion& region, std::uint64_t my_uuid, std::uint64_t my_pid) noexcept {
    std::uint64_t expected = 0;
    if (region.header.hub_uuid.compare_exchange_strong(expected, my_uuid,
                                                       std::memory_order_acq_rel)) {
        region.header.hub_pid.store(my_pid, std::memory_order_release);
        region.header.hub_heartbeat.store(0, std::memory_order_release);
        return true;
    }
    return false;
}

inline void releaseHub(SharedRegion& region, std::uint64_t my_uuid) noexcept {
    std::uint64_t expected = my_uuid;
    region.header.hub_uuid.compare_exchange_strong(expected, 0,
                                                   std::memory_order_acq_rel);
    region.header.hub_pid.store(0, std::memory_order_release);
}

} // namespace gatherer::protocol

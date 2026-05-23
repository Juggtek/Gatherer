#pragma once

#include <atomic>
#include <cstdint>

#include "ringbuffer/SpscRingBuffer.h"

// Wire-level layout of the named shared memory region that satellites and the hub
// use to communicate. See DESIGN.md §5 for the full protocol.
//
// **Strictly JUCE-free.** Only the C++ standard library and other `common/` headers.

namespace gatherer::protocol {

inline constexpr const char*   SHM_NAME           = "gatherer.shm.v1";

inline constexpr std::uint32_t MAGIC              = 0x47544852u; // 'GTHR'
inline constexpr std::uint32_t PROTOCOL_VERSION   = 1u;

// Sized for the stub PoC. DESIGN.md targets 64; smaller here keeps the shm region
// modest while we validate the architecture. Bumping requires a protocol version bump.
inline constexpr std::uint32_t NUM_SLOTS          = 16u;
inline constexpr std::uint32_t RING_FRAMES        = 8192u;  // power of two; ~170ms @ 48k
inline constexpr std::uint32_t RING_CHANNELS      = 2u;

inline constexpr std::uint32_t SLOT_STATE_EMPTY   = 0u;
inline constexpr std::uint32_t SLOT_STATE_CLAIMED = 1u;
inline constexpr std::uint32_t SLOT_STATE_ACTIVE  = 2u;

// Header. Created once by whichever process creates the shm region; readers wait
// for `init_done` to become 1 before trusting any other field.
struct Header {
    std::uint32_t              magic;               // MAGIC after init
    std::uint32_t              version;             // PROTOCOL_VERSION after init
    std::uint64_t              shm_size_bytes;
    std::uint32_t              num_slots;
    std::uint32_t              channels_per_slot;

    std::atomic<std::uint32_t> init_done;           // 0 = uninit, 1 = ready
    std::atomic<std::uint32_t> instance_refcount;   // # plugin instances attached

    std::atomic<std::uint64_t> hub_uuid;            // 0 = no hub registered
    std::atomic<std::uint64_t> hub_pid;
    std::atomic<std::uint64_t> hub_heartbeat;

    std::atomic<std::uint32_t> sample_rate;         // last set by hub::prepareToPlay
    std::atomic<std::uint32_t> max_block_size;

    // Active calibration probe. Hub bumps `calibration_session_id` and sets
    // `calibration_active = 1` to start a run. Each sat detects the new session
    // on its next processBlock and atomically records (hub_heartbeat, wp) into
    // its slot. Hub waits ~200ms, sets active=0, then compares records across
    // sats. Inter-sat divergence in recorded hub_heartbeat indicates the sat
    // ran in a different hub callback (= callback-level misalignment).
    std::atomic<std::uint64_t> calibration_session_id;  // 0 = never run; monotonic
    std::atomic<std::uint32_t> calibration_active;      // 0 / 1
    std::uint32_t              _calibration_pad;        // alignment

    std::uint8_t               reserved[240];
};

// One satellite slot. Mirrors DESIGN.md §5.2 — slot state, identity, ring buffer.
struct SatelliteSlot {
    std::atomic<std::uint32_t> state;               // EMPTY / CLAIMED / ACTIVE

    std::atomic<std::uint64_t> sat_uuid;
    std::atomic<std::uint64_t> sat_pid;
    std::atomic<std::uint64_t> sat_heartbeat;

    char                       display_name[64];    // user-set in plugin UI
    char                       track_name[64];      // from host via updateTrackProperties
    std::uint32_t              color_rgba;

    SpscRingBuffer::Header     ring_header;

    // Linear mapping: host_frame_at_ring_position(p) = anchor_host_frame + p,
    // valid for p in [write_pos - capacity, write_pos). The satellite updates this
    // atomically only on first write or on detected playhead discontinuity (DAW seek),
    // so in continuous playback hub reads a STABLE value with no race against wp.
    std::atomic<std::int64_t>  anchor_host_frame;

    // Diagnostic only — last write's host frame end. Hub does NOT use this for alignment.
    std::atomic<std::int64_t>  last_write_host_frame;

    // Calibration response. Written by the satellite when it first sees a new
    // calibration_session_id in the header. cal_session_acked echoes the session
    // id; cal_start_hub_heartbeat captures the hub's heartbeat as the sat saw it
    // at that moment; cal_start_wp captures the sat's own ring write_pos.
    std::atomic<std::uint64_t> cal_session_acked;
    std::atomic<std::uint64_t> cal_start_hub_heartbeat;
    std::atomic<std::uint64_t> cal_start_wp;

    // PDC-calibration solo control. Set by hub to silence this sat's output
    // (sat continues to write its captured audio into the ring as normal,
    // but clears its passthrough output buffer). Hub uses this during the
    // per-sat PDC measurement: it mutes every sat except the one being
    // measured, so the parent-bus mix → hub's input contains *only* the
    // target sat's content. Cross-correlating sat's SHM stream against that
    // mix becomes a near-trivial match, giving sample-accurate D.
    std::atomic<std::uint32_t> cali_mute_output;     // 0 = pass through, 1 = output zeros

    float                      ring_data[RING_FRAMES * RING_CHANNELS];

    std::uint8_t               reserved[228];
};

struct SharedRegion {
    Header        header;
    SatelliteSlot slots[NUM_SLOTS];
};

// Initialize the region after first creation. Safe to call only when the caller is
// the shm owner (isOwner() == true). Publishes `init_done = 1` last via release store.
inline void initializeNewRegion(SharedRegion& r) noexcept {
    r.header.version           = PROTOCOL_VERSION;
    r.header.num_slots         = NUM_SLOTS;
    r.header.channels_per_slot = RING_CHANNELS;
    r.header.shm_size_bytes    = sizeof(SharedRegion);

    for (auto& slot : r.slots) {
        SpscRingBuffer::initialize(slot.ring_header);
    }

    // Write magic before init_done so a reader that observes init_done = 1
    // is guaranteed to read magic = MAGIC.
    r.header.magic = MAGIC;
    r.header.init_done.store(1u, std::memory_order_release);
}

inline bool isInitialized(const SharedRegion& r) noexcept {
    return r.header.init_done.load(std::memory_order_acquire) == 1u
        && r.header.magic == MAGIC
        && r.header.version == PROTOCOL_VERSION;
}

} // namespace gatherer::protocol

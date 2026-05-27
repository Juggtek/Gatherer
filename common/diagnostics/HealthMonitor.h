#pragma once

// Health / misalignment detector for the Gatherer hub.
//
// Lives in the JUCE-free `common/` library so the same analyzer can be reused later
// by a standalone hub process. Reads only metadata that's already in the shared region
// (heartbeats, write_pos, last_write_host_frame); never touches audio data.
//
// Single-threaded: meant to be ticked from the hub's UI thread at ~10Hz. Not safe to
// share an instance across threads.

#include <algorithm>
#include <array>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <deque>
#include <limits>
#include <string>
#include <vector>

#include "../protocol/SharedRegion.h"

namespace gatherer::diagnostics {

enum class HealthLevel {
    Unknown,   // Not enough data yet (startup, no sats)
    Green,     // Aligned and tracking
    Yellow,    // Working but suspicious (idle DAW, rate divergence within tolerance, etc.)
    Red,       // Real misalignment detected
};

struct HealthStatus {
    HealthLevel level = HealthLevel::Unknown;
    std::string summary;  // one-liner shown next to the badge
    std::string detail;   // longer explanation, what the user should try
};

struct SlotSample {
    bool          active                = false;
    std::uint64_t uuid                  = 0;
    std::uint64_t heartbeat             = 0;
    std::uint64_t write_pos             = 0;
    std::int64_t  last_write_host_frame = 0;
};

struct Sample {
    std::chrono::steady_clock::time_point at;
    std::uint64_t hub_heartbeat = 0;
    std::uint32_t max_block_size = 0;
    std::array<SlotSample, gatherer::protocol::NUM_SLOTS> slots{};
};

class HealthMonitor {
public:
    void tick(const Sample& s) {
        history_.push_back(s);
        while (history_.size() > kWindow) history_.pop_front();
        recompute();
    }

    HealthStatus current() const noexcept { return current_; }

    void clear() noexcept {
        history_.clear();
        current_ = {};
    }

private:
    // ~3 seconds of history at the expected 10Hz tick rate.
    static constexpr std::size_t kWindow = 30;
    // Rate is computed over up to this much wall time of recent samples.
    static constexpr double      kRateWindowSeconds = 2.0;
    // A slot whose heartbeat is advancing slower than this is treated as a "ghost" —
    // claimed-but-not-running. Could be an orphaned slot from a destroyed plugin
    // instance, or a sat on a muted/bypassed track that isn't getting callbacks.
    // Excluded from rate divergence comparisons (they would always look like Red).
    static constexpr double      kLiveSlotMinRate = 1.0;

    std::deque<Sample> history_;
    HealthStatus       current_;

    void recompute() {
        using sec = std::chrono::duration<double>;

        if (history_.size() < 2) {
            current_ = {HealthLevel::Unknown, "Collecting data...", ""};
            return;
        }

        const auto& newest = history_.back();

        // Pick the oldest sample that falls inside our rate window.
        const Sample* oldest = &history_.front();
        for (const auto& s : history_) {
            if (sec(newest.at - s.at).count() <= kRateWindowSeconds) {
                oldest = &s;
                break;
            }
        }
        const double dt = sec(newest.at - oldest->at).count();
        if (dt < 0.3) {
            current_ = {HealthLevel::Unknown, "Collecting data...", ""};
            return;
        }

        const double hub_rate = ratePerSec(newest.hub_heartbeat,
                                           oldest->hub_heartbeat, dt);

        // Slots that were claimed throughout the window with the same UUID. We then
        // split these into "live" (heartbeat actually advancing) and "ghost" (claimed
        // but not running) — the latter are excluded from rate comparisons.
        std::vector<int> claimed;
        for (std::size_t i = 0; i < gatherer::protocol::NUM_SLOTS; ++i) {
            if (newest.slots[i].active && oldest->slots[i].active &&
                newest.slots[i].uuid == oldest->slots[i].uuid &&
                newest.slots[i].uuid != 0) {
                claimed.push_back(static_cast<int>(i));
            }
        }

        if (claimed.empty()) {
            current_ = {HealthLevel::Yellow,
                        "No active satellites",
                        "Add satellite plugins on the tracks you want to gather."};
            return;
        }

        if (hub_rate < 1.0) {
            current_ = {HealthLevel::Yellow,
                        "DAW transport idle",
                        "Press play in the host to start streaming audio."};
            return;
        }

        // Filter to live slots (heartbeat actually advancing).
        std::vector<int> live;
        std::vector<int> ghosts;
        double min_rate = std::numeric_limits<double>::infinity();
        double max_rate = 0.0;
        int slowest = -1, fastest = -1;
        for (int slot : claimed) {
            const double r = ratePerSec(newest.slots[slot].heartbeat,
                                         oldest->slots[slot].heartbeat, dt);
            if (r >= kLiveSlotMinRate) {
                live.push_back(slot);
                if (r < min_rate) { min_rate = r; slowest = slot; }
                if (r > max_rate) { max_rate = r; fastest = slot; }
            } else {
                ghosts.push_back(slot);
            }
        }

        if (live.empty()) {
            current_ = {HealthLevel::Yellow,
                        "Satellites idle",
                        "Slots are claimed but no processBlock callbacks are being received. "
                        "Tracks may be muted, bypassed, or routed away from the playback chain."};
            return;
        }

        // Build a hint to append to whatever status we settle on, if there are ghosts.
        std::string ghost_hint;
        if (!ghosts.empty()) {
            ghost_hint = fmt(" Ignoring stale slot %d (no callbacks).", ghosts.front());
        }

        // Tolerance: 5% of the hub rate, or 1 callback per second, whichever is larger.
        const double rate_tol = std::max(1.0, hub_rate * 0.05);

        if (live.size() > 1 && (max_rate - min_rate) > rate_tol) {
            current_ = {
                HealthLevel::Yellow,
                "Satellites called at different rates",
                fmt("Slot %d runs %.0f/s, slot %d runs %.0f/s. ",
                    fastest, max_rate, slowest, min_rate) +
                "Common causes: per-track clip-gating (Bitwig only calls "
                "processBlock on tracks with an active clip at the playhead), "
                "host pre-rolling for PDC, or parallel-track scheduling. "
                "Per-track PDC offsets compensate for this; recordings should "
                "still align." + ghost_hint};
            return;
        }

        const double hub_vs_sat = std::fabs(hub_rate - max_rate);
        if (hub_vs_sat > rate_tol) {
            current_ = {
                HealthLevel::Yellow,
                "Hub call rate diverges from satellites",
                fmt("Hub: %.0f/s, sats: %.0f/s. ", hub_rate, max_rate) +
                "Hub may be skipping callbacks (track has no audio source) or "
                "host is render-ahead-calling one side. Audio output still aligns "
                "via the duplicate-call guard." + ghost_hint};
            return;
        }

        // LWH spread check, only across live slots.
        if (live.size() > 1) {
            std::int64_t lwh_min = std::numeric_limits<std::int64_t>::max();
            std::int64_t lwh_max = std::numeric_limits<std::int64_t>::min();
            bool any = false;
            for (int slot : live) {
                const auto lwh = newest.slots[slot].last_write_host_frame;
                if (lwh > 0) {
                    any = true;
                    if (lwh < lwh_min) lwh_min = lwh;
                    if (lwh > lwh_max) lwh_max = lwh;
                }
            }
            if (any) {
                const std::int64_t spread = lwh_max - lwh_min;
                const std::int64_t tol    =
                    std::max<std::int64_t>(64, newest.max_block_size / 2);
                if (spread > tol) {
                    current_ = {
                        HealthLevel::Red,
                        "Satellites at different timeline positions",
                        fmt("LWH spread = %lld samples (~%d block(s)). ",
                            static_cast<long long>(spread),
                            newest.max_block_size > 0
                                ? static_cast<int>(spread / newest.max_block_size)
                                : 0) +
                        "Polarity null between satellites will not cancel. "
                        "Move the hub onto a parent group/bus track." + ghost_hint};
                    return;
                }
            }
        }

        std::string ok_msg = "All running satellites track in lockstep with the hub.";
        ok_msg += ghost_hint;
        current_ = {HealthLevel::Green, "Aligned", ok_msg};
    }

    static double ratePerSec(std::uint64_t newer, std::uint64_t older, double dt) {
        if (dt <= 0.0) return 0.0;
        const auto delta = (newer >= older) ? (newer - older) : 0;
        return static_cast<double>(delta) / dt;
    }

    // printf-style formatter into std::string (small fixed buffer; never throws).
    template <typename... Args>
    static std::string fmt(const char* f, Args... args) {
        char buf[256];
        std::snprintf(buf, sizeof(buf), f, args...);
        return std::string(buf);
    }
};

} // namespace gatherer::diagnostics

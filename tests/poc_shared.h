#pragma once

#include <atomic>
#include <cstdint>

namespace gatherer::poc {

inline constexpr const char* SHM_NAME = "gatherer.poc";
inline constexpr std::uint32_t MAGIC  = 0x47504F43; // 'GPOC'

// Simple layout for the SHM PoC: writer publishes a monotonically increasing counter
// and the reader verifies it advances. `ready` gates the reader until the writer has
// completed initialization.
struct PocLayout {
    std::atomic<std::uint32_t> magic;
    std::atomic<std::uint32_t> ready;
    std::atomic<std::uint64_t> counter;
    std::atomic<std::uint64_t> last_value;
    char pad[64];
};

} // namespace gatherer::poc

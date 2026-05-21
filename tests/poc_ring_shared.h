#pragma once

#include <atomic>
#include <cstdint>

#include "ringbuffer/SpscRingBuffer.h"

namespace gatherer::poc {

inline constexpr const char*   RING_SHM_NAME = "gatherer.poc.ring";
inline constexpr std::uint32_t RING_MAGIC    = 0x47524E47;  // 'GRNG'
inline constexpr std::uint32_t RING_CAPACITY = 4096;        // power of two
inline constexpr std::uint32_t RING_CHANNELS = 2;

struct RingLayout {
    std::atomic<std::uint32_t>   magic;
    std::atomic<std::uint32_t>   ready;
    SpscRingBuffer::Header       rb_header;
    float                        data[RING_CAPACITY * RING_CHANNELS];
};

} // namespace gatherer::poc

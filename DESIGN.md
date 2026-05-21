# Gatherer — Design Document

**Status:** Draft v0.1
**Authors:** Felix + Claude
**Last updated:** 2026-05-19

---

## 1. Goals

Build an audio plugin system that gathers parallel audio streams from many DAW tracks into one place, where they can be recorded, measured, normalized, mixed adaptively, and exported.

The system has two plugins working together:
- **Satellite** — small plugin instantiated on each source track. Taps the track's audio and forwards it to the hub.
- **Hub** — central plugin instantiated once. Receives all satellite streams, mixes them to its output, records, meters, normalizes, exports.

A **standalone hub build** also exists, where instead of receiving from satellites it reads audio directly from a multi-channel audio interface (or files), with each input channel acting as a "layer."

### Phase 1 (MVP)
- Satellite gathering with stable cross-session pairing
- Hub mixes all satellite streams to its stereo output, sample-aligned
- Per-stream metering: LUFS (I/S/M), true peak, peak, RMS
- Continuous recording of all streams (per-satellite WAV files)
- Offline normalization (target LUFS or peak)
- Stem export (per-satellite WAVs) and optional summed mix export
- Standalone hub with `AudioDeviceManager` multi-input routing

### Phase 2
- **Adaptive Mixer** — formula-based per-layer gain model, smoothed, applied before the sum
- **Sections** — Intro / Main / Outro markers; section-scoped recording
- **Internal playback** — transport in the hub, scrub recorded layers, solo
- **Custom export format** — folder bundle: stem WAVs + `manifest.json` (markers, gains, metadata)

## 2. Non-goals

- Commercial distribution (internal use only)
- AAX support
- Surround / multichannel beyond stereo in v1
- Cross-machine / network audio
- Latency-free monitoring through the hub (PDC will add ≤1 block of latency)
- VST2

## 3. High-level architecture

```
                       ┌───────────────────────────────────┐
                       │       Named Shared Memory         │
                       │  ┌─────────────────────────────┐  │
                       │  │ Header (version, hub info)  │  │
                       │  ├─────────────────────────────┤  │
                       │  │ Slot 0: ringbuf + metadata  │  │
                       │  │ Slot 1: ringbuf + metadata  │  │
                       │  │ ...                          │  │
                       │  │ Slot N-1                    │  │
                       │  └─────────────────────────────┘  │
                       └───────────────────────────────────┘
                                ▲                    ▲
                                │ write              │ read
                                │                    │
   ┌──────────┐   ┌──────────┐  │   ┌─────────────┐  │
   │ Track 1  │──►│ Satellite│──┘   │             │  │
   └──────────┘   └──────────┘      │     Hub     │──┘
   ┌──────────┐   ┌──────────┐      │ (recorder,  │
   │ Track 2  │──►│ Satellite│─────►│  meters,    │
   └──────────┘   └──────────┘      │  mixer,     │
   ┌──────────┐   ┌──────────┐      │  exporter)  │
   │ Track 3  │──►│ Satellite│─────►│             │
   └──────────┘   └──────────┘      └─────────────┘
```

Each satellite and hub is a separate plugin binary loaded by the DAW. They share state through OS-level **named shared memory**, not via globals — because separate `.vst3` bundles do not share static storage even within the same process.

## 4. Plugin formats, platforms, build

- **Formats:** VST3, Standalone (hub only)
- **Platforms:** macOS (universal: x86_64 + arm64), Windows 10/11 (x86_64), Linux (x86_64)
- **Framework:** JUCE 8.x (GPL-licensed use, internal only)
- **Build:** CMake, JUCE pulled in via `FetchContent` or submodule
- **Compiler baseline:** C++20
- **Architecture targets:**
  - macOS: clang, arm64 + x86_64 universal binary
  - Windows: MSVC 2022, x64
  - Linux: gcc 11+ or clang 14+, x86_64

### Crate / target layout

```
gatherer/
├── CMakeLists.txt
├── third_party/
│   ├── JUCE/                   (submodule, GPL)
│   └── libebur128/             (submodule, MIT — LUFS measurement)
├── common/                     STATIC LIB, no JUCE deps in public headers
│   ├── shm/                    cross-platform shared-memory wrapper
│   ├── protocol/               shared memory layout structs + protocol version
│   ├── ringbuffer/             SPSC ring buffer over shm-backed memory
│   ├── registry/               satellite registry, claim/release, heartbeat
│   ├── meter/                  LUFS / peak / RMS wrappers
│   ├── recorder/               background WAV writer
│   ├── normalize/              offline pass
│   ├── adaptive_mixer/         Phase 2: formula-based gain
│   ├── sections/               Phase 2: marker model
│   └── manifest/               Phase 2: project format read/write
├── satellite/                  → gatherer-sat.vst3
├── hub/                        → gatherer-hub.vst3 + gatherer-hub (standalone)
└── tests/                      unit tests, GoogleTest
```

**The `common/` library is strictly JUCE-free** — not in public headers, not in implementation. JUCE may be linked only by the `satellite/` and `hub/` targets. The reason is licensing flexibility: JUCE is GPL-or-paid-commercial, and keeping the core free of it means the core can be relicensed, open-sourced under a permissive license, or reused outside the plugin context (e.g. a future Rust port or a standalone CLI tool) without inheriting JUCE's terms.

Concrete consequences:
- WAV I/O in core uses `dr_wav.h` (public domain), not `juce::WavAudioFormat`.
- UUID generation in core uses `<random>` / a small lib, not `juce::Uuid`.
- Lock-free primitives in core are written here (or pulled in as permissive header-only), not `juce::AbstractFifo`.
- Threads, file I/O, atomics in core use the C++ standard library.
- All new core dependencies must be MIT / BSD / Apache / public-domain.

This boundary also keeps `common/` unit-testable without launching a DAW.

## 5. Inter-plugin communication

### 5.1 Shared memory object

A single named shared memory region holds the entire protocol state.

- **Name:** `gatherer.shm.v1` (per protocol version)
  - macOS/Linux: `shm_open("/gatherer.shm.v1", ...)`
  - Windows: `CreateFileMappingW(... L"Local\\gatherer.shm.v1")`
- **Lifecycle:** created on first access (lazy), unlinked when no plugin instances remain (refcount via shm header).
- **Size:** computed from `NUM_SLOTS × sizeof(SatelliteSlot) + sizeof(Header)`. For 64 slots × stereo × 48000 frames at 4 bytes = ~24 MB. Acceptable.

### 5.2 Shared memory layout

All fields are POD, fixed-size, with `std::atomic<T>` for any field that's read-while-written. **No pointers** — pointers are not portable across mappings of the same shm in different DLLs at potentially different addresses. Use indices and offsets only.

```cpp
namespace gatherer {

constexpr uint32_t MAGIC = 0x47544852;     // 'GTHR'
constexpr uint32_t PROTOCOL_VERSION = 1;
constexpr uint32_t NUM_SLOTS = 64;
constexpr uint32_t RING_FRAMES = 48000;    // 1 sec @ 48k, scaled at runtime if SR differs
constexpr uint32_t RING_CHANNELS = 2;      // stereo v1

struct Header {
    uint32_t magic;
    uint32_t version;
    uint64_t shm_size_bytes;

    std::atomic<uint32_t> instance_refcount;  // # of plugin instances attached
    std::atomic<uint64_t> hub_uuid;           // 0 if no hub present
    std::atomic<uint64_t> hub_pid;
    std::atomic<uint64_t> hub_heartbeat;      // hub increments per block

    uint32_t sample_rate;                     // set by hub on first prepare
    uint32_t max_block_size;
    uint32_t num_slots;
    uint32_t channels_per_slot;

    uint8_t reserved[256];
};

struct SatelliteSlot {
    // 0 = empty, 1 = claimed, 2 = active (heartbeat fresh)
    std::atomic<uint32_t> state;

    std::atomic<uint64_t> sat_uuid;
    std::atomic<uint64_t> sat_pid;
    std::atomic<uint64_t> sat_heartbeat;    // satellite increments per block

    // User-visible identity (set by satellite, read by hub)
    char     display_name[64];               // user-set label in plugin UI, null-terminated UTF-8
    char     track_name[64];                 // DAW track name from host, null-terminated UTF-8
    uint32_t color_rgba;

    // SPSC ring buffer (single producer = satellite, single consumer = hub)
    std::atomic<uint64_t> write_pos;         // monotonic frame index, sat writes
    std::atomic<uint64_t> read_pos;          // monotonic frame index, hub reads
    std::atomic<int64_t>  last_write_host_frame; // host playhead at last write

    float ring_data[RING_FRAMES * RING_CHANNELS];

    uint8_t reserved[256];
};

struct SharedRegion {
    Header header;
    SatelliteSlot slots[NUM_SLOTS];
};

} // namespace gatherer
```

### 5.3 Registry protocol

**Satellite startup (in `prepareToPlay` or first `processBlock`):**
1. Read its persistent UUID from plugin state (generated on first instantiation).
2. Iterate `slots[]`. If a slot has matching `sat_uuid`, reclaim it (CAS state from any → 1).
3. Otherwise, find any slot with `state == 0` and CAS to 1, write `sat_uuid`, `sat_pid`, `name`.
4. CAS state 1 → 2 once ring buffer is initialized.
5. On every `processBlock`, increment `sat_heartbeat`.

**Hub startup:**
1. CAS `hub_uuid` from 0 to its own UUID (only one hub allowed). If non-zero and PID is dead → reclaim.
2. Set `sample_rate`, `max_block_size`.
3. On every `processBlock`, increment `hub_heartbeat`, iterate active slots, read available frames, mix.

**Satellite shutdown:**
1. CAS state 2 → 0, clear UUID. (Don't clear ring data — leave for next reclaim.)

**Hub shutdown:**
1. CAS `hub_uuid` to 0.

**Stale detection:**
- During hub's per-block sweep, if a slot's `sat_heartbeat` hasn't changed in N blocks (say 1 second worth), mark it inactive in hub state (but don't clear the slot — satellite may be bypassed or its track muted).
- During satellite's per-block run, if `hub_heartbeat` is stale, satellite keeps writing — hub will catch up when it returns.

**PID liveness:**
- On registration, check if the PID stored in a stale slot is still alive (kill(pid, 0) on POSIX, OpenProcess on Win32). If dead, reclaim the slot.

### 5.4 Ring buffer

Lock-free SPSC, indexed (no pointers). Built on `juce::AbstractFifo`'s pattern but with our own `write_pos`/`read_pos` atomics for shm visibility.

- **Producer (satellite):** writes `block_size × channels` floats per `processBlock`. Increments `write_pos`. If buffer would overflow (hub far behind), advance `read_pos` to drop oldest frames — better than blocking the audio thread.
- **Consumer (hub):** reads up to `available = write_pos - read_pos` frames. Increments `read_pos`.
- **Sizing:** 1 second at 48 kHz stereo = 384 KB per slot. Enough to absorb DAW scheduling jitter and brief glitches.

Memory barriers: `std::memory_order_release` on writes to `write_pos`, `std::memory_order_acquire` on reads. The `ring_data` writes must happen-before the `write_pos` store. Standard SPSC pattern.

### 5.5 Timing and latency model

The trickiest part. We need hub's output to be a sample-aligned mix of all satellite inputs, regardless of DAW topology.

**The problem:** within one audio callback, the DAW calls `processBlock` on each plugin in some order. If satellite[t] is upstream of hub (e.g. hub on master bus), satellite runs before hub for block N — hub can read block N. If satellite[t] is on a separate parallel track, order is unspecified — hub may run before satellite for block N.

**The solution: hub reads one block in arrears, reports 1 block of latency via PDC.**

- Hub calls `setLatencySamples(max_block_size)`.
- When hub's `processBlock` runs for host playhead range `[p, p + bs)`, it outputs the sum of satellite data for `[p - bs, p)`.
- The host's PDC ensures the hub's delayed output aligns with other tracks at the master bus.
- Order-of-call within a block becomes irrelevant: satellites for `[p - bs, p)` were called in the *previous* audio callback, guaranteed before hub's current callback.

**Frame indexing:**
- Each satellite tags its writes with the host playhead frame (`AudioPlayHead::getPosition().timeInSamples`).
- Hub, when reading, requests "the frames written for host range `[p - bs, p)`" — finds them in each satellite's ring by walking back from `write_pos` using `last_write_host_frame`.
- This handles tracks with different latency (e.g. instrument plugins reporting latency) — the *content* is aligned to host frames, not to wall-clock writes.

**Failure modes:**
- A satellite hasn't written for the target range (track is silent, plugin bypassed, satellite added mid-song): hub substitutes silence for that slot in that block.
- A satellite's ring overruns: hub gets a discontinuity. Logged; UI shows a warning indicator on that slot.

**Standalone mode:**
- No DAW, no PDC, no satellites. Hub reads from `AudioDeviceManager` callback directly. Each input channel pair → one "virtual layer." Frame index = monotonic counter starting at zero.

### 5.6 Pairing and session persistence

- **Satellite state** (saved in `getStateInformation`):
  - persistent UUID (16 bytes, generated on first instantiation)
  - display name (user-set label, e.g. "Kick", "Bass")
  - track name (last value reported by host via `AudioProcessor::updateTrackProperties` — useful for hub UI fallback when the user hasn't set a display name, and for matching after a re-bind)
  - color
- **Hub state:**
  - list of "known satellites": UUID → (name, color, gain, mute, solo, etc.)
  - section markers (Phase 2)
  - adaptive mixer parameters (Phase 2)

When a session reopens:
- Each satellite reconstructs from its saved UUID and reclaims its shm slot.
- Hub iterates `slots[]`, matches active UUIDs against its saved list. UI shows reconnected layers as "linked," any saved-but-missing satellites as "offline."

**Rebinding:** if the user duplicates a track (creating two satellites with the same persistent state and thus same UUID — DAW-dependent), the second satellite generates a new UUID on the fly and notifies the hub UI of an "unknown new satellite." User can rebind.

## 6. Hub processing pipeline

Per `processBlock`:

1. Increment `hub_heartbeat`.
2. Sweep `slots[]`:
   - For each active slot, determine target frame range `[p - bs, p)`.
   - Read ring buffer; if not enough data, fill missing with silence.
   - Write into a per-slot scratch buffer.
3. **Phase 2:** Apply Adaptive Mixer formula → per-slot gain → smoothed.
4. **Phase 1:** Apply manual per-slot gain / mute / solo from hub UI.
5. **Metering:** push each slot's scratch buffer into its `Meter` (lock-free; meter computes on a low-priority thread).
6. **Recording:** if armed, hand each slot's scratch buffer to the `Recorder` (lock-free FIFO to writer thread).
7. **Mix:** sum all slot scratch buffers into hub output (stereo).

All allocations / file I/O / locks happen off the audio thread.

## 7. Standalone hub

- Same hub plugin, compiled with JUCE's `StandalonePluginHolder`.
- Replaces shm-based input with `AudioDeviceManager`.
- UI adds an "Audio Settings" panel using `AudioDeviceSelectorComponent`.
- Channel routing: a mapping table `[input channel pair] → [virtual slot]`. User configures, persisted in app settings.
- Optional: file-input mode. Drop WAVs onto slots; transport plays them through the same pipeline. (Useful for offline measurement/normalization passes.)

## 8. Phase 1 components

### 8.1 Metering
- `libebur128` for LUFS-Momentary, LUFS-Short, LUFS-Integrated, True Peak.
- Custom peak and RMS computed inline.
- Meter state updated from a worker thread reading from a per-slot FIFO; UI reads atomically.

### 8.2 Recorder
- Per-armed-slot WAV writer using `juce::WavAudioFormat`.
- Audio thread writes into a lock-free FIFO; background `WriterThread` drains to disk.
- 32-bit float WAV by default (no clipping, normalization is offline).
- Filename pattern: `<session_dir>/<slot_name>_<timestamp>.wav`.

### 8.3 Normalizer (offline)
- Scan recorded WAVs, compute target gain to hit configured LUFS-I or peak target.
- Two modes: per-file independent, or relative (preserve relative levels, anchor to loudest).
- Writes new files alongside originals; never overwrites.

### 8.4 Exporter
- Per-stem WAV (already on disk from recorder; just copy/transcode).
- Optional summed mix WAV.
- Export dialog: choose destination, format (WAV 16/24/32), bit depth, sample rate (resample if needed via `juce::Interpolators`).

## 9. Phase 2 sketch

### 9.1 Adaptive Mixer
- Per-slot feature extraction every block: RMS, crest factor, optional spectral centroid.
- Formula evaluates per slot → target gain.
- Smoother: one-pole low-pass on gain, attack/release configurable (separately, like a compressor).
- Gain applied in the hub processing pipeline before sum.
- UI: per-slot gain curve viewer over time; formula parameters; bypass.

### 9.2 Sections
- `Section` = { name, start_frame, end_frame, color }.
- Stored in hub state.
- Recording engine respects section-arm: writes only frames within an armed section's range.
- Timeline component in hub UI for placing/dragging sections.

### 9.3 Internal playback
- `juce::AudioTransportSource` + `AudioFormatReaderSource` per recorded layer.
- Transport in hub UI: play/stop, scrub, loop section, solo a layer.
- During playback, hub UI bypasses live shm input and feeds the playback pipeline through the same metering / mixing / export path.

### 9.4 Custom export format
- Folder bundle: `<project>.gatherer/`
  - `manifest.json` — version, sample rate, duration, sections, per-layer metadata, adaptive mixer state
  - `layers/<uuid>.wav` — one stem per layer
  - Optional `mix.wav` — pre-rendered summed mix
- Simple, debuggable, easy to round-trip.

## 10. Testing strategy

- **Unit tests** (GoogleTest) on `common/`:
  - Shared memory wrapper (create / open / map / unmap)
  - Registry claim/release under simulated concurrent access
  - Ring buffer correctness and overrun behavior
  - Meter accuracy against known sine signals
  - Recorder + Normalizer round-trip
- **Integration tests:**
  - Two console processes — one writes, one reads, verify sample-accurate transfer through shm
  - Mock JUCE `AudioProcessor` host to exercise plugin lifecycle without a DAW
- **Manual DAW validation matrix** (for the PoC and each release):
  - Reaper, Bitwig, Logic, Cubase, Studio One — at least Reaper + Bitwig + Logic for PoC
  - Verify: PDC alignment with click track, session reload preserves pairings, duplicate-track behavior, removing satellite mid-session

## 11. Open questions & risks

1. **PDC reliability across DAWs.** Logic in particular is known to handle PDC inconsistently for plugins that report latency from a non-FX bus. **Mitigation:** verify in PoC week.
2. **Shared memory across separate `.vst3` bundles.** Same DAW process but different DLLs. Confirmed possible on all three platforms but quirks exist (e.g. macOS sandboxing for some hosts). **Mitigation:** PoC verifies on Reaper / Bitwig / Logic; document any sandboxed hosts as unsupported.
3. **Plugin scanning lifecycle.** DAWs scan plugins by instantiating them briefly. The shm registry must tolerate rapid create/destroy without leaving stale slots. **Mitigation:** PID-based liveness check on slot reclaim.
4. **DAW-duplicated tracks producing satellites with the same persistent UUID.** Some DAWs duplicate plugin state verbatim. **Mitigation:** on second registration of a UUID with a different PID + still-active heartbeat, generate a new UUID.
5. **PDC with non-fixed block sizes.** Some DAWs vary block size per call. **Mitigation:** report worst-case latency = max block size declared at `prepareToPlay`.
6. **Sample-rate or block-size changes mid-session.** Re-initialize shm slot ring buffers on `prepareToPlay`. Drop currently-buffered data (with a warning in UI).
7. **64 slot limit.** Reasonable for the use case; if it becomes too low, bump and version the protocol.

## 12. Out of scope (explicit non-decisions for v1)

- Network / cross-machine satellites
- Inter-plugin audio at sub-sample precision (sample-aligned is enough)
- Cross-platform automation of standalone audio device config
- Custom plugin signing / notarization (internal use — local signing only as needed)

## 13. PoC week — exit criteria

Before committing to the full Phase 1 build, the PoC must demonstrate:

1. Named shared memory works in: macOS arm64, macOS x86_64, Windows x64, Linux x64.
2. Two stub VST3 plugins (sat + hub) attach to the same shm in **Reaper, Bitwig, and Logic** on macOS.
3. A 1 kHz sine wave on a satellite track appears at the hub output, sample-aligned (verified by null test: hub output inverted against sat input → silence within float epsilon).
4. Two satellites mix correctly at hub.
5. Reloading the session reconnects sat → hub pairings.
6. Adding / removing satellites mid-session works without crashing.
7. Standalone hub launches, opens an audio device, mixes two input channel pairs.

If any of 1–6 fail, redesign before proceeding. (7 can slip without blocking Phase 1.)

---

## Appendix A — Naming

- `gatherer-sat` — satellite VST3
- `gatherer-hub` — hub VST3 + Standalone
- Shared memory name: `gatherer.shm.v1`
- Project file extension: `.gatherer` (Phase 2 folder bundle)

## Appendix B — Library / dependency choices

Dependencies are split by layer. The `common/` core uses only permissively-licensed libraries (MIT / BSD / Apache / public domain) so it remains relicensable and JUCE-free. JUCE is restricted to the plugin shells.

**Core (`common/`) — permissive only:**

| Need | Choice | License | Why |
|---|---|---|---|
| LUFS measurement | libebur128 | MIT | EBU R128 reference implementation, plain C |
| WAV I/O | dr_wav.h | Public domain | Single-header, no dependencies, well-tested |
| Lock-free SPSC | Hand-written, this repo | (project) | Tiny, fits the shm/index pattern exactly |
| Shared memory | Hand-written, this repo | (project) | Thin wrapper over `shm_open` / `CreateFileMappingW` |
| JSON (manifest, Phase 2) | nlohmann/json | MIT | Header-only, ubiquitous |
| UUID generation | Hand-written (RFC 4122 v4 via `<random>`) | (project) | Trivial; avoids any framework dep |
| Tests | GoogleTest | BSD-3 | Integrates with CMake |

**Plugin shells (`satellite/`, `hub/`) — JUCE-bound:**

| Need | Choice | Why |
|---|---|---|
| Plugin framework | JUCE 8.x (GPL) | Industry standard, AI-buildable, mature GUI, standalone target |
| Standalone audio device UI | `juce::AudioDeviceManager` + selector component | Out-of-the-box multi-channel routing |
| Plugin GUI primitives | `juce::Component` and friends | JUCE's strongest layer |

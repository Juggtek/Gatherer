# Gatherer — Architecture (as-built)

This document describes the **inter-plugin audio transport** as it actually exists in the codebase today (Phase 1 PoC complete), the reasoning that led to its current shape, the topology required for it to work reliably, and the known weaknesses and improvement directions.

For the forward-looking plan (Phase 1/2 features, milestones, etc.) see [DESIGN.md](DESIGN.md). This document is descriptive, not prescriptive.

---

## 1. The transport, in one diagram

```
                          ┌──────────────────────────────────────┐
                          │   gatherer.shm.v1   (POSIX shm)      │
                          │  ┌────────────────────────────────┐  │
                          │  │ Header                         │  │
                          │  │   magic, version, hub_uuid,    │  │
                          │  │   hub_heartbeat, ...           │  │
                          │  ├────────────────────────────────┤  │
                          │  │ Slot 0:  uuid, ring_header,    │  │
                          │  │          ring_data[8192*2]     │  │
                          │  ├────────────────────────────────┤  │
                          │  │ Slot 1:  ...                   │  │
                          │  │   ...                          │  │
                          │  │ Slot 15: ...                   │  │
                          │  └────────────────────────────────┘  │
                          └──────────────────────────────────────┘
                                       ▲             ▲
                                       │ write       │ peek
                                       │             │
   ┌───────────────────────┐           │             │      ┌──────────────────────┐
   │  Satellite (per sat)  │───────────┘             └──────│   Hub (one)          │
   │  - claims a slot      │                                │  - claims hub_uuid   │
   │  - rb.write each      │                                │  - rb.peekAt each    │
   │    processBlock       │                                │    processBlock      │
   │  - pass-through       │                                │  - clears input,     │
   │    audio              │                                │    sums into output  │
   └───────────────────────┘                                └──────────────────────┘
```

The shared memory object is a fixed-layout struct (`gatherer::protocol::SharedRegion`) mapped into every plugin's address space. All plugins of the same DAW process see the same memory.

The protocol is JUCE-free: it lives in `common/` and only uses `<atomic>`, `<cstdint>`, and POSIX/Win32 for the shm wrapper itself. JUCE only appears in the plugin shells (`satellite/`, `hub/`).

---

## 2. Components, top-down

### 2.1 `common/shm/SharedMemory`
Cross-platform named shared memory wrapper. POSIX (`shm_open` + `mmap`) and Win32 (`CreateFileMappingW` + `MapViewOfFile`). Does **not** unlink on destruction — the OS name persists for the process lifetime, which is essential so that a satellite created later can find the same region a hub already mapped. Manual cleanup via `SharedMemory::unlink(name)` or the `gatherer-reset` CLI.

### 2.2 `common/protocol/SharedRegion`
The wire-format struct. Fixed sizes, all integers little-endian (we assume same-architecture host process for any sat ↔ hub pair, which is true: they're loaded into the same DAW). Header carries magic/version/hub identity. `slots[16]` array carries per-satellite state and audio ring.

### 2.3 `common/ringbuffer/SpscRingBuffer`
Power-of-two-sized lock-free single-producer single-consumer ring of interleaved floats. 8192 frames × 2 channels per slot. Monotonic `write_pos` and `read_pos` published with release/acquire semantics.

Key methods:
- `write(src, frames)` — producer side. Includes an **overrun policy** that advances `read_pos` if it would otherwise outrun the consumer. (See §5 for why this both helps and complicates things.)
- `read(dst, frames)` — consumer side. Loads `read_pos`, copies, stores new `read_pos`. **Hub does not currently use this**; see §3.3.
- `peekAt(position, dst, frames)` — random-access read at an arbitrary monotonic position, **without touching `read_pos`**. Returns false if data not yet written or already overwritten. Hub uses this.
- `setReadPos(pos)` — explicit positioning, still writes to the shared `read_pos`. Used during earlier iterations of the hub but no longer.

### 2.4 `common/protocol/Registry`
Claim/release operations on the shm header and slots, using CAS. Single source of truth for "who is the hub" and "which sat owns which slot."

### 2.5 Plugin shells (`satellite/`, `hub/`)
Thin JUCE wrappers. Each owns a `SharedMemory` mapping and a pointer into the `SharedRegion`. Satellites claim a slot on `prepareToPlay` (or first `processBlock`); hub claims `hub_uuid`. Both release in their destructors.

### 2.6 Tools (`tools/`)
- `gatherer-watch` — read-only watcher CLI. Polls the shm at 100ms and prints `write_pos`, `read_pos`, lag, heartbeat per slot. Used as the primary debug surface throughout the audio-alignment work.
- `gatherer-reset` — `shm_unlink` on the region name. Use when state has gone stale (e.g., DAW crash left a hub_uuid set with a dead pid).

---

## 3. The audio path — what hub `processBlock` actually does

The transport model went through five iterations during the PoC. Here's the final one, plus the reasoning for each rejection.

### 3.1 What we do now

In hub's `processBlock`, for each active slot:

```cpp
const auto wp = rb.writePos();                       // acquire

if (state.last_uuid != uuid) {                       // slot reclaimed by new sat
    state.last_uuid    = uuid;
    state.last_seen_wp = 0;
}

if (wp == state.last_seen_wp) continue;              // dup-call → silence this buffer
state.last_seen_wp = wp;

if (wp < target_lag) continue;                       // sat hasn't produced enough yet

if (!rb.peekAt(wp - target_lag,                      // peek at "1 block ago"
               scratch_.data(), frames)) continue;

// sum into output buffer
```

`target_lag = max_block_size_` (the value passed to the most recent `prepareToPlay`). Hub declares the same number via `setLatencySamples(samplesPerBlock)` so the DAW's PDC compensates.

The key properties:
- **Re-anchored every callback.** The read position is recomputed from current `wp` and current `target_lag` on every call. We do not cache a `local_rp` or rely on initialization-time state.
- **`peekAt` only — never writes the shared `read_pos`.** Hub leaves `read_pos` alone; it's managed entirely by the satellite's overrun policy.
- **Duplicate-call guard.** If Reaper calls hub's `processBlock` again without the sat advancing `wp` (which it does, e.g., for render-ahead), the hub outputs silence for that buffer rather than re-reading the previous block. Re-reading caused audible duplicate-sample clicks on transients.

### 3.2 Why the satellite is simple

`SatelliteProcessor::processBlock` does the obvious thing: pack input into interleaved scratch, call `rb.write`, pass the input through unchanged. No playhead checks, no `isPlaying` gating, no anchor-update bookkeeping. Every iteration that put logic on the sat side caused a bug somewhere.

### 3.3 The five rejected models (and what each taught us)

| Iteration | Approach | Why it failed |
|---|---|---|
| 1. Naive FIFO drain | `rb.read(frames)` per callback, no setup. | Lag = whatever ring happened to contain at hub start (often `capacity` ≈ 170ms). Audible delay; inter-sat lag offsets. |
| 2. Playhead-indexed via `last_write_host_frame` | Compute ring position from current host frame and per-sat `(wp, lwh)` snapshot. | (a) Reaper's anticipative FX produces non-monotonic host frames, so the discontinuity-detection in the sat fired every block and constantly updated the anchor. (b) (wp, lwh) snapshot not atomic across threads → consumer saw inconsistent pairs → 1-block offsets manifesting as crackling. |
| 3. Anchor (set once) + playhead | Sat sets `anchor` on first write only; never updates. Hub computes ring pos from `(playhead - latency) - anchor`. | Worked in theory. Failed in practice when sat received its very first `processBlock` call during monitoring (not playing) — anchor got set against a stale or zero playhead. |
| 4. `setReadPos` every callback + `rb.read` | Force `read_pos = wp - target_lag` every call. Read advances `read_pos`. | Worked for alignment! But: (a) racy with sat's overrun policy on the shared atomic, (b) Reaper double-calls produced duplicate-sample reads because `read_pos` got reset and `rb.read` returned the same data. Audible clicks on transients. |
| 5. `peekAt` with cached `local_rp` | Per-slot local read position, advanced by `frames` each call. | Worked until the latch caught a stale `target_lag` value from an early `prepareToPlay`. Reaper calls `prepareToPlay` more than once with different `samplesPerBlock` values; the first one is sometimes a very large render-ahead buffer (~7000+ samples). Initialization captured that and never recovered. |

The current model (§3.1) is iteration 6: re-anchored `peekAt` with a `last_seen_wp` guard. It composes the working parts of iterations 4 and 5 while avoiding both of their failure modes.

---

## 4. Topology that's actually required

The transport is reliable when **the hub sits downstream of every satellite in the DAW's audio graph** — i.e., hub on a parent group/bus track or on the master, sats on child tracks routed up through it.

In that topology, every satellite's `processBlock` is guaranteed to have run for the current Reaper audio callback by the time the hub's `processBlock` runs. The hub peeks at `wp - target_lag` and gets a freshly-written block from every sat, all corresponding to the same Reaper callback → polarity null between two sats with the same content cancels cleanly.

**Confirmed working in Reaper and Bitwig** with that topology. Test 1 (alignment with direct passthrough) and Test 3 (polarity null between two sats) both pass.

### When parallel tracks don't work, and why

If sats are on tracks parallel to the hub (none downstream of any other), the within-callback plugin execution order is unspecified. Some sats may run before the hub, some after. The ones that ran before have their current block in the ring; the ones that didn't, don't. Hub reads at `wp - target_lag` from each → some get current-block content, some get previous-block content → a constant 1-block offset between sats → no polarity null, audible flam.

This is **not a fix-from-inside-the-plugin** problem. The hub has no way to wait for the late-running sats — it returns from `processBlock` and the audio engine moves on. The only ways out are:
- Tell the user "use hierarchy" (current approach)
- Run on a host-provided coordination primitive (none exists in VST3/AU/CLAP)
- Build the hub as a standalone process that decouples from the DAW's callback graph (see §6.1)

### The Reaper "parallel-after-hierarchy" quirk

Once a project has had a hierarchy topology connected, Reaper appears to cache the PDC values, and switching to parallel can sometimes keep working *during the same session*. After a project reload, parallel-only fails again. The workaround is **Track → Send/receive routing → Recalculate PDC**.

---

## 5. Known weaknesses

### 5.1 Parallel topology is unreliable
Documented in §4. Mitigation: recommend hierarchy in setup docs. We could detect this case (see §6.2).

### 5.2 No seek handling
Hub doesn't know about DAW playhead. If the user stops, seeks to a different position, and plays again, satellites continue writing into their rings at the next `wp`, but the hub keeps reading the most-recent `target_lag` samples from each. The audio content is correct (just sat's recent input audio), but if a Phase 2 feature needs to align hub output to a specific timeline position (e.g., recording with timeline-correct file timestamps), the seek will cause an artifact at the transition.

Mitigation deferred to Phase 2 once we actually need timeline-correct recording.

### 5.3 Reaper anticipative FX produces non-monotonic playhead values
Discovered during iteration 2 (§3.3). We sidestepped it by removing playhead-dependent code from the audio path entirely, so this is currently not a bug — but any Phase 2 feature that wants to use the host playhead in the audio path will hit this again. Pre-disable anticipative FX, or filter for monotonic HFS, or accept seek-style discontinuities at every anticipation event.

### 5.4 Hub blocks input audio
`buffer.clear()` at the top of `HubProcessor::processBlock` discards whatever the host hands the hub as input. This is the correct default for the hierarchy topology (avoids double-counting sat passthrough at master) but means **Test 1 (sat passthrough vs hub output) is not testable when hub is downstream**. If a user wants pass-through-plus-mix behavior ("monitor mode"), the fix is a one-line `add` instead of `clear` — but the default should stay as it is.

### 5.5 Satellite's overrun policy writes the shared `read_pos`
When the ring fills, the satellite advances `read_pos` to drop the oldest data. This is correct ring-buffer behavior for the case where the producer outruns the consumer, but it competes with anyone else who might want to write `read_pos` (a previous iteration of the hub did, leading to a race; the current hub doesn't, so this is currently safe). If a future change makes the hub write to `read_pos` again, the race comes back.

### 5.6 No misalignment detection
The hub does not currently know whether its output is actually aligned. If the user is in a parallel topology, the hub continues to produce output silently — no warning. The user has to A/B compare with direct passthrough or run a polarity null test to discover the problem. This is the next obvious gap; see §6.2.

### 5.7 PDC matching is unverified across hosts
We've confirmed alignment in Reaper and Bitwig with hierarchy topology. We haven't tested Logic, Live, Studio One, FL, or Cubase. Some hosts handle PDC differently (Live in particular has historically been quirky about plugin-reported latency).

### 5.7a Parallel-topology block-rate race
Even when the user **listens only to the hub's output** (so PDC isn't involved at master), parallel topology produces audible artifacts because of a race in how Reaper schedules parallel tracks against the hub:

- In each Reaper audio callback, hub's `processBlock` reads each sat's ring at `wp_x - target_lag`.
- In hierarchy topology, sats are processed *before* the hub deterministically. Every sat has completed writing for callback K before hub reads. All `wp - target_lag` reads return callback K-1 content. Aligned.
- In parallel topology, Reaper processes tracks (potentially on multiple threads). Sat 0 may finish before hub reads while sat 1 doesn't, or vice versa. Hub then mixes `sat0[K-1]` + `sat1[K-2]` — different Reaper-time content — and the null test shows block-rate artifacts.

The audio cross-correlation in the calibration probe **averages** over an 85ms window (8 blocks). Across that window the gross content alignment looks perfect, so the audio probe reports 0-sample offset. But the per-block reads in `processBlock` race anew each callback, producing intermittent misalignment that the windowed correlation can't see.

The **callback-level probe** does see this: when sats see calibration in different `hub_heartbeat` values, that's the same race manifesting at calibration-detection time. Disagreement between the two probes (callback-level says misaligned, audio-correlation says aligned) is the diagnostic fingerprint of parallel-topology race.

Fixing this from inside the plugin requires per-block atomic `(hub_hb, wp)` publication from each sat (seqlock) plus a hub-side per-callback consensus on which "Reaper callback" to read from — significant protocol work, and even then it would add latency. **Recommended workaround:** use hierarchy topology. Document it as the only sample-accurate configuration.

### 5.8 Stereo only
Ring is hardcoded to 2 channels. Surround / mono / arbitrary channel layouts are not supported.

### 5.9 Fixed slot count
16 slots. Adding satellites beyond that silently fails (the claim returns "no free slot"). Compile-time constant; not user-configurable.

### 5.10 Ghost slots (claimed but not running)
A slot can end up in state `ACTIVE` with no plugin actually backing it. Two ways this happens:

1. **Ungraceful plugin destruction** — Reaper kills the plugin process / aborts a scan / unloads ungracefully. The destructor doesn't run, so `releaseSlot` is never called, and the slot remains marked claimed.
2. **Plugin scan + project load race** — a scan instance claims a slot during plugin inventory, then the real project-load instance gets a different slot. The scan instance's slot may or may not be cleaned up depending on how Reaper handles its lifecycle.

Symptoms: `gatherer-watch` shows the slot as ACTIVE but `hb` is frozen at some past value while other slots advance. The hub's audio path produces silence for that slot (reading the same `wp - target_lag` position every callback, with no new data) — so it doesn't actively break alignment, but it does cause the health monitor to false-positive (which is why the monitor now filters ghosts out, per §6.2).

**Workaround today:** run `./build/tools/gatherer-reset` between sessions to wipe the shm clean.

**Proper fix (future):** hub-side reaping. If a slot is `ACTIVE` but its heartbeat hasn't advanced for ~2 seconds while the hub *itself* is advancing (i.e., audio is flowing), the hub CAS-releases the slot. This is safe because a legitimately-running sat would be incrementing its heartbeat. The only false positive is "DAW transport stopped, then resumed" — which would either advance all slots equally (no reap) or advance none (no reap because hub also stops).

---

## 6. Improvement directions

### 6.1 Unified hub deployment (VST3 plugin + Standalone app)

**Single `HubProcessor`, two deployments.** JUCE's build pipeline produces both `Gatherer Hub.vst3` (in-DAW) and `Gatherer Hub.app` (standalone macOS app, with the equivalent for Win/Linux) from the same `AudioProcessor` source. The standalone wrapper handles the OS audio device (Core Audio / WASAPI / ASIO / JACK), surfaces a device-picker UI, and calls the same `processBlock` with input/output buffers driven by the system instead of by a host. The `HubEditor` is loaded identically in both — every feature added to the plugin shows up in the standalone with no extra code.

**One deployment-aware parameter**: `include_track_input` (persisted in state).

- **OFF** (default for the in-DAW VST3): `processBlock` clears its input buffer and outputs only the sat-ring mix. Correct for the recommended hub-on-parent-bus topology — the sat tracks route their audio up through the hub's track, but we don't want it summed twice with the hub's mix at master.
- **ON** (default for Standalone): the input buffer is kept, and the sat-ring mix is summed on top. In standalone the input buffer *is* the system audio input (mic / line / virtual loopback device), so discarding it would defeat the purpose.

Default is chosen at construction time via `wrapperType == wrapperType_Standalone`, then saved/restored with project state so user changes stick.

**No code duplication between deployments.** All Phase 1/2 features — recording, metering, normalization, adaptive mixer, export — live in `HubProcessor` + `HubEditor` and benefit both deployments automatically. The deployment-specific surface is:

- The single `include_track_input` default
- JUCE's standalone wrapper UI (device picker etc.) which we get for free

**Standalone-specific concerns** (still no fundamental architectural changes needed):

- Reading sat rings: the standalone is **immune to the parallel-topology race** (§5.7a). It runs outside the DAW's audio callback, so when its own audio callback reads `wp_x - target_lag` from each sat's ring, the data is already comfortably in the past — all sats have finished writing it. As long as both sats are called at the same rate per Reaper callback (the existing health monitor flags it if they aren't), the standalone gets sample-accurate aligned reads.
- Channel layout: standalone supports arbitrary input/output channel counts via the audio device config. The plugin stays stereo (simplest interop with hosts). Users who want multi-channel routing into the hub use the standalone with a virtual audio device (BlackHole/VB-Cable) bridging DAW outputs.

**Standalone known limitations (defer)**:

- JUCE's default standalone wrapper exposes only a single stereo input device pair. Multi-input from several sources at once (e.g. multiple mic preamps as independent slots) needs either a custom wrapper or aggregating via a virtual audio device. Workaround today: use BlackHole / Loopback Pro / VB-Audio to aggregate, then pick the aggregate device in the standalone's device picker.
- Audio output sometimes doesn't reach the system output in the default device config — likely a JUCE standalone wrapper init quirk that the user has to fix in the device dialog (Audio → Device Settings → pick output device explicitly). Investigate when we polish standalone packaging.

The plugin shell remains "the natural in-DAW deployment", but it's not architecturally required for the hub's coordinator role — the standalone covers everything the plugin does plus system-audio input, and the shm is keyed by name so either deployment can be the registered hub for any session.

**The standalone is immune to the parallel-topology race (§5.7a)**, because that race exists specifically due to the in-DAW hub competing with sats for execution order *within* a Reaper audio callback. A standalone process polling shm is outside that timing constraint entirely. With a read-back lag of ~50ms (≈ 5 audio callbacks), the data being read is well in the past and has definitively been written by every active sat regardless of their within-callback timing.

The one assumption that still applies to the standalone: **all active sats must be called the same number of times per Reaper callback.** Anticipative-FX schedulers that pre-render some sats and not others will drift them apart — but the existing heartbeat-rate divergence check in the health monitor catches that for the standalone just as well as it does for the in-DAW hub.

Practically:
- **Standalone for recording (Phase 1)**: use 50–100ms read-back lag. Capture each sat to its own WAV. Sample-accurate within the latency tolerance. No protocol changes needed.
- **Standalone for real-time adaptive mixing (Phase 2)**: latency depends on use case. If the mixer is being driven by analysis (formula computed from incoming audio), the latency is the analysis-window length anyway. Should also work without protocol changes.
- **If a future use case truly demands low-latency standalone with no race risk**: that's when the per-block atomic publication (seqlock) becomes warranted. Not before.

### 6.2 Misalignment detection — **IMPLEMENTED (passive)**

The hub now self-checks alignment every UI tick (~10Hz) using shm metadata only. No audio path changes, no protocol changes. Lives in [common/diagnostics/HealthMonitor.h](common/diagnostics/HealthMonitor.h) as a JUCE-free analyzer so the standalone hub (§6.1) can reuse it.

**Signals consulted** (in order of severity):

1. **`sat_heartbeat` rate divergence between *live* sats** — if Sat A advances faster than Sat B (Reaper called A anticipatively, or only some sats are downstream of an upstream effect), they're not in lockstep. Reported as `Red`.
2. **`hub_heartbeat` vs sat heartbeat rate** — if hub is being called at a different rate from sats, hub is skipping callbacks (track has no audio source) or sats are getting double-calls. Reported as `Yellow` (audio output guard handles it but topology is suspect).
3. **`last_write_host_frame` spread across live sats at the same hub callback** — sats should have all written for the same host frame within ~½ block. If they're off by ≥ ½ block, the within-callback ordering is broken (parallel-topology symptom). Reported as `Red`.

**Live vs ghost filtering.** A slot whose `sat_heartbeat` rate is below ~1/s while playback is active is considered a "ghost" — claimed-but-not-running. Ghosts are excluded from divergence comparisons (otherwise their `rate = 0` would always read as misalignment vs. any running sat). The status detail mentions the ghost slot so the user knows to investigate. See §5.10 for the underlying issue.

**UI surface** (in the hub editor): colored badge — green/yellow/red — with a one-line summary, a detail explanation that names the specific symptom and the user-fixable cause, and a **Re-analyze** button that wipes the rolling-window history (useful right after a topology change so detection stabilizes immediately rather than over 1–2 seconds).

Rolling window: 2 seconds of history at 10Hz. Rate computation uses the first sample within that window; states resolve in ~1 second after a topology change (or immediately with Re-analyze).

**Active calibration probe — IMPLEMENTED**.

When the passive metadata check is uncertain (parallel topology, sub-block timing differences that the LWH spread tolerance lets through, or the user just wants ground truth), the hub editor's **Calibrate** button runs an active probe:

1. Hub bumps `header.calibration_session_id` (unique per run) and sets `calibration_active = 1`.
2. Each satellite, on its very next `processBlock` where it sees a session id it hasn't acked, atomically snapshots `(hub_heartbeat, write_pos)` into its slot and acks the session id.
3. After ~250ms, hub clears `calibration_active` and reads each slot's snapshot.
4. **Inter-sat offset** = max(`hub_hb_at_ack`) − min(`hub_hb_at_ack`) across responding sats. Zero = all sats detected the session in the same hub callback (= callback-level alignment). Non-zero = sats are running in different hub callbacks; the offset translates directly to samples via the current `max_block_size`.

This catches misalignments the passive monitor can't, including:

- Sub-block-but-non-zero deviations where the LWH spread happens to fall within the ½-block tolerance but the satellites are nonetheless in different audio callbacks.
- Cases where `last_write_host_frame` reporting is broken (Reaper anticipative FX, hosts that don't expose playhead reliably).
- Single-shot verification after a topology change — no need to wait for the rolling window to stabilize.

The result is displayed in the editor as a second badge (green = aligned, red = misaligned) with a detail line naming the involved slots and the offset in callbacks and samples.

**Future detection layers** (still not built):

| Signal | What it'd add | Cost |
|---|---|---|
| Cross-ring audio correlation | Direct sample-level offset measurement from the audio data itself, not from metadata. Catches sub-sample drift the callback-counter probe can't see. | Moderate — FFT correlation on the next ~1 second after a calibration trigger; ideally combined with sat-side known-signal injection so the correlation has a sharp peak |
| Auto-correction | Use the probe result to set per-sat `read_offset` in the hub's audio path, compensating for measured misalignment | Requires the seqlock work for atomic `(wp, lwh)` snapshot, plus a UI affordance for "apply correction" |

### 6.3 Hub UI as full mixer
The current hub editor is a placeholder. Phase 1 plan calls for it to grow into the actual mix surface — per-sat meters, gain, mute/solo, record-arm, transport, file export. This is the next user-facing milestone.

### 6.4 Seek-aware recording
Once we want to record to disk with timeline-correct file timestamps, the recording layer needs to know the host playhead. Adding a per-callback playhead snapshot to the slot (alongside the data) lets the recorder annotate file regions with timeline positions. This is independent of the audio routing path and doesn't reintroduce the playhead issues from iteration 2/3 — the audio path stays purely callback-clocked, only the *metadata* uses the playhead.

### 6.5 Channel layouts beyond stereo
The ring buffer takes `channels_` as a constructor argument but the protocol's `RING_CHANNELS` is currently a hardcoded 2. Lifting that to per-slot (variable-size slots), or just to a session-wide constant, is straightforward but a wire-format change.

### 6.6 Larger rings
8192 frames at 48k = ~170ms of ring capacity. This is plenty for the normal 1-block latency, but if we ever want to allow hub to fall further behind (e.g., for offline normalization passes that pull more data), we'd need bigger rings. Trivial change.

### 6.7 Cross-platform validation
Need to confirm the transport works in Logic, Live, Cubase, Studio One, FL. The risk is PDC interpretation differences (Live especially) and AU-vs-VST3 host-call-order differences.

---

## 7. Things we explicitly decided not to do (and why)

- **Playhead-driven audio routing.** Tried twice (iterations 2 and 3). Reaper's anticipative FX makes the host playhead untrustworthy as a per-block timestamp. The current callback-clocked model is more robust.
- **Atomic snapshot of `(wp, lwh)`.** Considered seqlock / 16-byte atomic / packed encoding to make `(wp, lwh)` snapshot-consistent. Abandoned when iteration 6 removed the dependency on `lwh` entirely.
- **AAX support.** Avid SDK is closed; no Rust binding exists, and even C++/JUCE AAX requires PACE/iLok signing for production load. Out of scope for internal use.

---

## 8. Reference: where each idea lives in code

| Concept | File |
|---|---|
| shm wrapper | [common/shm/SharedMemory.h](common/shm/SharedMemory.h), [.cpp](common/shm/SharedMemory.cpp) |
| Wire format | [common/protocol/SharedRegion.h](common/protocol/SharedRegion.h) |
| Registry (claim/release) | [common/protocol/Registry.h](common/protocol/Registry.h) |
| Ring buffer | [common/ringbuffer/SpscRingBuffer.h](common/ringbuffer/SpscRingBuffer.h) |
| Satellite plugin | [satellite/PluginProcessor.h](satellite/PluginProcessor.h), [.cpp](satellite/PluginProcessor.cpp) |
| Hub plugin | [hub/PluginProcessor.h](hub/PluginProcessor.h), [.cpp](hub/PluginProcessor.cpp) |
| Watcher CLI | [tools/watcher.cpp](tools/watcher.cpp) |
| Reset CLI | [tools/reset.cpp](tools/reset.cpp) |
| Forward-looking design | [DESIGN.md](DESIGN.md) |

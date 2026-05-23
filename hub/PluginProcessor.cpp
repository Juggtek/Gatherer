#include "PluginProcessor.h"
#include "PluginEditor.h"

#include "protocol/Registry.h"

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <string>
#include <thread>
#include <vector>

#if defined(_WIN32)
    #include <windows.h>
    static std::uint64_t currentPid() { return static_cast<std::uint64_t>(::GetCurrentProcessId()); }
    static bool isPidAlive(std::uint64_t pid) {
        if (pid == 0) return false;
        HANDLE h = ::OpenProcess(SYNCHRONIZE, FALSE, static_cast<DWORD>(pid));
        if (h == nullptr) return false;
        const auto r = ::WaitForSingleObject(h, 0);
        ::CloseHandle(h);
        return r == WAIT_TIMEOUT;
    }
#else
    #include <unistd.h>
    #include <signal.h>
    #include <errno.h>
    static std::uint64_t currentPid() { return static_cast<std::uint64_t>(::getpid()); }
    static bool isPidAlive(std::uint64_t pid) {
        if (pid == 0) return false;
        // kill(pid, 0): 0 = alive; -1 with ESRCH = no such process; EPERM = exists but no perm.
        if (::kill(static_cast<pid_t>(pid), 0) == 0) return true;
        return errno == EPERM;
    }
#endif

using namespace gatherer;
using namespace gatherer::protocol;

namespace {
constexpr float kGainDbMin = -60.0f;
constexpr float kGainDbMax =  12.0f;

inline float dbToLin(float db) {
    if (db <= -99.0f) return 0.0f;
    return std::pow(10.0f, db / 20.0f);
}
inline float linToDb(float lin) {
    if (lin <= 1e-7f) return -100.0f;
    return 20.0f * std::log10(lin);
}
}

HubProcessor::HubProcessor()
    : juce::AudioProcessor(BusesProperties()
        .withInput("Input",   juce::AudioChannelSet::stereo(), true)
        .withOutput("Output", juce::AudioChannelSet::stereo(), true)),
      session_(*this)
{
    for (std::size_t i = 0; i < display_order_.size(); ++i)
        display_order_[i] = static_cast<int>(i);

    // PDC: start all sat estimates and overrides as unknown so the UI can
    // show "not measured yet" until cross-correlation produces a value.
    for (auto& a : pdc_d_samples_)   a.store(kPdcUnknown, std::memory_order_relaxed);
    for (auto& a : pdc_d_override_)  a.store(kPdcUnknown, std::memory_order_relaxed);

    my_uuid_ = generateInstanceId();
    // Standalone defaults to summing system-audio input with the sat mix (otherwise
    // the standalone has no use for its audio device input). In-DAW VST3 defaults to
    // discarding the track's input (matches the hub-on-parent-bus topology, which is
    // the recommended usage).
    include_track_input_.store(wrapperType == wrapperType_Standalone,
                               std::memory_order_relaxed);

    // Set up AudioThumbnail infrastructure. Format manager only needs basic
    // formats registered (we feed thumbnails via addBlock, no file reading,
    // but the API requires a manager).
    thumb_format_manager_.registerBasicFormats();
    for (std::size_t i = 0; i < thumbnails_.size(); ++i) {
        thumbnails_[i] = std::make_unique<juce::AudioThumbnail>(
            /*sourceSamplesPerThumbSample*/ 512,
            thumb_format_manager_,
            thumb_cache_);
    }

    attachToShm();
}

HubProcessor::~HubProcessor() {
    // Stop background workers first — they read sat rings via region_, so they
    // must be torn down before detachFromShm clears the region pointer.
    if (pdc_calibrator_) {
        pdc_calibrator_->signalThreadShouldExit();
        pdc_calibrator_->stopThread(1000);
        pdc_calibrator_.reset();
    }
    if (lufs_worker_) {
        lufs_worker_->signalThreadShouldExit();
        lufs_worker_->stopThread(1000);
        lufs_worker_.reset();
    }
    // Stop any in-flight recording before detaching from the shared region —
    // the writer threads read the satellite rings via region_ pointers.
    if (recorder_) recorder_->stop();
    recorder_.reset();
    normalizer_.reset();  // joins its own thread in dtor
    detachFromShm();
}

void HubProcessor::attachToShm() {
    try {
        shm_ = std::make_unique<SharedMemory>(SHM_NAME, sizeof(SharedRegion),
                                              SharedMemory::Mode::OpenOrCreate);
        region_ = static_cast<SharedRegion*>(shm_->data());

        if (shm_->isOwner()) {
            initializeNewRegion(*region_);
        } else {
            const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
            while (!isInitialized(*region_)) {
                if (std::chrono::steady_clock::now() > deadline) break;
                std::this_thread::sleep_for(std::chrono::milliseconds(10));
            }
        }

        region_->header.instance_refcount.fetch_add(1, std::memory_order_acq_rel);
        is_hub_ = claimHub(*region_, my_uuid_, currentPid());

        // One-shot cleanup of stale slots left over from prior host processes
        // (e.g., a Bitwig session that exited without destructing its sat
        // plugins). PID-based; live sats stay untouched.
        if (is_hub_) reclaimGhostSlots();
    } catch (const std::exception&) {
        shm_.reset();
        region_ = nullptr;
        is_hub_ = false;
    }
}

void HubProcessor::detachFromShm() {
    if (region_) {
        if (is_hub_) releaseHub(*region_, my_uuid_);
        region_->header.instance_refcount.fetch_sub(1, std::memory_order_acq_rel);
        region_ = nullptr;
    }
    shm_.reset();
    is_hub_ = false;
}

void HubProcessor::prepareToPlay(double sampleRate, int samplesPerBlock) {
    max_block_size_ = samplesPerBlock;
    scratch_.assign(static_cast<std::size_t>(samplesPerBlock) * RING_CHANNELS, 0.0f);

    // Stop the worker before touching its analyzers — restart afterwards.
    if (lufs_worker_) {
        lufs_worker_->signalThreadShouldExit();
        lufs_worker_->stopThread(1000);
        lufs_worker_.reset();
    }
    for (auto& sl : lufs_) {
        sl.analyzer = std::make_unique<gatherer::measurement::LoudnessAnalyzer>(2, sampleRate);
        sl.integrated .store(-100.0f, std::memory_order_relaxed);
        sl.momentary  .store(-100.0f, std::memory_order_relaxed);
        sl.short_term .store(-100.0f, std::memory_order_relaxed);
        sl.last_fed_wp   = 0;
        sl.query_counter = 0;
    }
    lufs_worker_ = std::make_unique<LufsWorker>(*this);
    lufs_worker_->startThread();

    // PDC measurement: allocate the mono reference ring and (re)start the
    // calibrator. The ring stores hub's PDC-aligned input audio; the worker
    // periodically cross-correlates each sat against this reference to
    // estimate per-sat D.
    if (pdc_calibrator_) {
        pdc_calibrator_->signalThreadShouldExit();
        pdc_calibrator_->stopThread(1000);
        pdc_calibrator_.reset();
    }
    pdc_ref_data_.assign(static_cast<std::size_t>(kPdcRefCapacity), 0.0f);
    gatherer::SpscRingBuffer::initialize(pdc_ref_header_);
    pdc_ref_anchor_set_.store(false, std::memory_order_release);
    pdc_calibrator_ = std::make_unique<PdcCalibrator>(*this);
    pdc_calibrator_->startThread();

    playback_.prepare(sampleRate, samplesPerBlock);

    // Report 1-block PDC latency so the host can compensate. The hub may receive a
    // satellite's data either same-block (if upstream) or next-block (if parallel);
    // declaring max_block_size of latency gives a deterministic upper bound.
    setLatencySamples(samplesPerBlock);

    if (region_ != nullptr && is_hub_) {
        region_->header.sample_rate.store(static_cast<std::uint32_t>(sampleRate),
                                          std::memory_order_release);
        region_->header.max_block_size.store(static_cast<std::uint32_t>(samplesPerBlock),
                                             std::memory_order_release);
    }
}

void HubProcessor::releaseResources() {
    if (pdc_calibrator_) {
        pdc_calibrator_->signalThreadShouldExit();
        pdc_calibrator_->stopThread(1000);
        pdc_calibrator_.reset();
    }
    if (lufs_worker_) {
        lufs_worker_->signalThreadShouldExit();
        lufs_worker_->stopThread(1000);
        lufs_worker_.reset();
    }
}

void HubProcessor::LufsWorker::run() {
    while (!threadShouldExit()) {
        processor_.lufsWorkerTick();
        wait(50);  // ~20Hz poll — plenty for LUFS metering
    }
}

void HubProcessor::lufsWorkerTick() {
    if (region_ == nullptr) return;

    // Read up to ~40ms of audio per tick at typical 48k. Ring capacity is
    // ~170ms so we'll never overrun even with the 50ms poll interval.
    constexpr std::uint32_t kChunkFrames = 2048;
    static thread_local std::vector<float> chunk;
    if (chunk.size() < static_cast<std::size_t>(kChunkFrames) * RING_CHANNELS) {
        chunk.resize(static_cast<std::size_t>(kChunkFrames) * RING_CHANNELS);
    }

    for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
        const auto& sat = region_->slots[i];
        if (sat.state.load(std::memory_order_acquire) != SLOT_STATE_ACTIVE) continue;

        auto& sl = lufs_[i];
        if (!sl.analyzer) continue;

        // Detect "new sat occupies this slot" (e.g. ghost reclaim then a fresh
        // claim, or first sat ever in this slot). Reset analyzer + anchor so
        // we don't bleed loudness history across sat lifetimes.
        const auto uuid = sat.sat_uuid.load(std::memory_order_acquire);
        if (uuid != sl.last_seen_uuid) {
            sl.analyzer->reset();
            sl.last_fed_wp    = 0;
            sl.last_seen_uuid = uuid;
            sl.query_counter  = 0;
            sl.integrated.store(-100.0f, std::memory_order_relaxed);
            sl.momentary .store(-100.0f, std::memory_order_relaxed);
            sl.short_term.store(-100.0f, std::memory_order_relaxed);
        }

        SpscRingBuffer rb(const_cast<SpscRingBuffer::Header&>(sat.ring_header),
                           const_cast<float*>(sat.ring_data),
                           RING_FRAMES, RING_CHANNELS);
        const auto wp_now = rb.writePos();

        // On first observation of a slot, anchor at its current wp — we don't
        // care about pre-attach audio.
        if (sl.last_fed_wp == 0) {
            sl.last_fed_wp = wp_now;
        }

        // Feed any new samples since last tick (in chunks).
        while (wp_now > sl.last_fed_wp) {
            const auto avail = wp_now - sl.last_fed_wp;
            const auto take  = static_cast<std::uint32_t>(
                std::min<std::uint64_t>(avail, kChunkFrames));
            if (!rb.peekAt(sl.last_fed_wp, chunk.data(), take)) {
                // Overrun — resync to current wp.
                sl.last_fed_wp = wp_now;
                break;
            }
            sl.analyzer->addInterleavedFloat(chunk.data(), take);
            sl.last_fed_wp += take;
        }

        // Query LUFS at ~4Hz (5 ticks × 50ms).
        if (++sl.query_counter >= 5) {
            sl.query_counter = 0;
            sl.integrated.store(
                static_cast<float>(sl.analyzer->integratedLufs()),
                std::memory_order_relaxed);
            sl.momentary.store(
                static_cast<float>(sl.analyzer->momentaryLufs()),
                std::memory_order_relaxed);
            sl.short_term.store(
                static_cast<float>(sl.analyzer->shortTermLufs()),
                std::memory_order_relaxed);
        }
    }
}

void HubProcessor::PdcCalibrator::run() {
    while (!threadShouldExit()) {
        if (processor_.solo_cali_active_.load(std::memory_order_acquire)) {
            processor_.runSoloCalibration();
            // runSoloCalibration completes a full sequence; clear the trigger.
            processor_.solo_cali_active_.store(false, std::memory_order_release);
        } else {
            processor_.pdcCalibratorTick();
        }
        wait(200);
    }
}

void HubProcessor::startSoloCalibration() {
    // Idempotent: only allow start if not already in progress.
    bool expected = false;
    if (!solo_cali_active_.compare_exchange_strong(expected, true,
                                                    std::memory_order_acq_rel)) {
        return;
    }
    solo_cali_state_.store(static_cast<std::uint8_t>(SoloCaliState::Capturing),
                            std::memory_order_release);
    {
        const juce::ScopedLock sl(solo_cali_message_lock_);
        solo_cali_message_ = "Starting solo calibration...";
    }
}

void HubProcessor::cancelSoloCalibration() {
    solo_cali_active_.store(false, std::memory_order_release);
}

HubProcessor::SoloCaliStatus HubProcessor::soloCaliStatus() const noexcept {
    SoloCaliStatus s;
    s.state           = static_cast<SoloCaliState>(solo_cali_state_.load(std::memory_order_relaxed));
    s.current_slot    = solo_cali_current_  .load(std::memory_order_relaxed);
    s.total_slots     = solo_cali_total_    .load(std::memory_order_relaxed);
    s.completed_slots = solo_cali_completed_.load(std::memory_order_relaxed);
    {
        const juce::ScopedLock sl(const_cast<juce::CriticalSection&>(solo_cali_message_lock_));
        s.message = solo_cali_message_;
    }
    return s;
}

// Iterates every active slot, soloes it by muting all other sats via the
// cali_mute_output SHM flag, lets the ref ring fill with ~600 ms of the
// isolated sat's content, then cross-correlates. Each measurement updates
// pdc_d_samples_ for that slot.
void HubProcessor::runSoloCalibration() {
    if (region_ == nullptr || !is_hub_) {
        solo_cali_state_.store(static_cast<std::uint8_t>(SoloCaliState::Failed),
                                std::memory_order_release);
        const juce::ScopedLock sl(solo_cali_message_lock_);
        solo_cali_message_ = "Hub not active.";
        return;
    }

    std::vector<int> active_slots;
    for (std::uint32_t i = 0; i < gatherer::protocol::NUM_SLOTS; ++i) {
        if (region_->slots[i].state.load(std::memory_order_acquire)
            == gatherer::protocol::SLOT_STATE_ACTIVE) {
            active_slots.push_back(static_cast<int>(i));
        }
    }
    solo_cali_total_    .store(static_cast<int>(active_slots.size()), std::memory_order_release);
    solo_cali_completed_.store(0, std::memory_order_release);

    if (active_slots.empty()) {
        solo_cali_state_.store(static_cast<std::uint8_t>(SoloCaliState::Failed),
                                std::memory_order_release);
        const juce::ScopedLock sl(solo_cali_message_lock_);
        solo_cali_message_ = "No active sats to calibrate.";
        return;
    }

    for (int slot : active_slots) {
        if (!solo_cali_active_.load(std::memory_order_acquire)) break;  // cancelled

        solo_cali_current_.store(slot, std::memory_order_release);
        solo_cali_state_  .store(static_cast<std::uint8_t>(SoloCaliState::Capturing),
                                  std::memory_order_release);
        {
            const juce::ScopedLock sl(solo_cali_message_lock_);
            solo_cali_message_ = "Calibrating slot " + std::to_string(slot) + "...";
        }

        const bool ok = measureOneSatSolo(slot);

        solo_cali_completed_.fetch_add(1, std::memory_order_release);
        if (!ok) {
            const juce::ScopedLock sl(solo_cali_message_lock_);
            solo_cali_message_ = "Slot " + std::to_string(slot)
                                + ": could not measure (sat silent or low correlation).";
        }
    }

    // Always restore: clear every cali_mute_output flag we touched.
    for (std::uint32_t i = 0; i < gatherer::protocol::NUM_SLOTS; ++i) {
        region_->slots[i].cali_mute_output.store(0u, std::memory_order_release);
    }

    solo_cali_state_  .store(static_cast<std::uint8_t>(SoloCaliState::Done),
                              std::memory_order_release);
    solo_cali_current_.store(-1, std::memory_order_release);
    {
        const juce::ScopedLock sl(solo_cali_message_lock_);
        solo_cali_message_ = "Solo calibration complete.";
    }
}

bool HubProcessor::measureOneSatSolo(int target_slot) {
    if (region_ == nullptr) return false;

    // Mute all sats except target.
    for (std::uint32_t i = 0; i < gatherer::protocol::NUM_SLOTS; ++i) {
        const std::uint32_t mute = (static_cast<int>(i) == target_slot) ? 0u : 1u;
        region_->slots[i].cali_mute_output.store(mute, std::memory_order_release);
    }

    // Strategy: stay on this sat indefinitely until we have a confident
    // measurement. No fixed timeout — user cancels via the Calibrate
    // button if a track has no audio in the project.
    //
    // Per iteration:
    //   1. Poll sat's ring for audible content (RMS above floor).
    //   2. When content appears, wait briefly so the ref ring fills, then
    //      run pdcCalibratorTick() to cross-correlate.
    //   3. Track the best confidence so far. If we beat it, remember the
    //      value. Repeat until confidence ≥ 0.5 or 5 confident
    //      measurements have converged (median).
    //
    // This is event-driven (we wait until audio is actually present)
    // and progress-tracked, so the UI can show "waiting for audio (8s)"
    // when a track is sparse, vs "measured 3 of 5 stable readings" when
    // we're getting close.

    constexpr int   kPollIntervalMs   = 100;
    constexpr int   kCaptureExtendMs  = 500;
    constexpr float kConfidentEnough  = 0.5f;
    constexpr float kAudibleRms       = 1e-4f;  // ~ -80 dBFS
    constexpr int   kProbeFrames      = 4096;   // ~85ms @ 48k
    constexpr int   kInterMeasureMs   = 400;    // between successive measurements
    constexpr int   kMaxMeasurements  = 10;     // cap takes per sat
    constexpr int   kWaitAudibleTimeoutMs = 15000;  // bail if a sat never produces audio

    const auto t_start = juce::Time::getMillisecondCounter();

    auto& slot = region_->slots[target_slot];
    SpscRingBuffer rb(slot.ring_header, slot.ring_data, RING_FRAMES, RING_CHANNELS);
    const auto wp_at_mute = rb.writePos();
    std::vector<float> probe(static_cast<std::size_t>(kProbeFrames) * RING_CHANNELS);

    auto check_audible = [&]() -> bool {
        const auto wp = rb.writePos();
        if (wp < wp_at_mute + static_cast<std::uint64_t>(kProbeFrames)) return false;
        const auto start_pos = wp - static_cast<std::uint64_t>(kProbeFrames);
        if (!rb.peekAt(start_pos, probe.data(), kProbeFrames)) return false;
        double s = 0.0;
        for (int i = 0; i < kProbeFrames * static_cast<int>(RING_CHANNELS); ++i) {
            s += static_cast<double>(probe[i]) * probe[i];
        }
        const double rms = std::sqrt(s / (kProbeFrames * RING_CHANNELS));
        return rms > kAudibleRms;
    };

    bool ever_heard         = false;
    int  measurement_count  = 0;
    float best_conf         = 0.0f;
    std::int64_t best_d_samples = 0;

    while (true) {
        if (!solo_cali_active_.load(std::memory_order_acquire)) return false;
        const auto elapsed = juce::Time::getMillisecondCounter() - t_start;

        // Refresh status.
        {
            const juce::ScopedLock sl(solo_cali_message_lock_);
            if (!ever_heard) {
                solo_cali_message_ = "Slot " + std::to_string(target_slot)
                                    + ": waiting for audio... ("
                                    + std::to_string(elapsed / 1000) + "s)";
            } else {
                solo_cali_message_ = "Slot " + std::to_string(target_slot)
                                    + ": measuring (best conf "
                                    + std::to_string(static_cast<int>(best_conf * 100))
                                    + "%, take "
                                    + std::to_string(measurement_count) + "/"
                                    + std::to_string(kMaxMeasurements) + ", "
                                    + std::to_string(elapsed / 1000) + "s)";
            }
        }

        if (!check_audible()) {
            // No audio yet. Give up only if we've waited a long time and
            // still haven't heard anything from this sat.
            if (!ever_heard && elapsed > static_cast<std::uint32_t>(kWaitAudibleTimeoutMs)) {
                const juce::ScopedLock sl(solo_cali_message_lock_);
                solo_cali_message_ = "Slot " + std::to_string(target_slot)
                                    + ": no audio detected, skipping.";
                return false;
            }
            juce::Thread::getCurrentThread()->wait(kPollIntervalMs);
            continue;
        }

        if (!ever_heard) {
            ever_heard = true;
            juce::Thread::getCurrentThread()->wait(kCaptureExtendMs);
            if (!solo_cali_active_.load(std::memory_order_acquire)) return false;
        }

        solo_cali_state_.store(static_cast<std::uint8_t>(SoloCaliState::Measuring),
                                std::memory_order_release);
        pdcCalibratorTick();
        ++measurement_count;

        const auto conf = pdc_confidence_[static_cast<std::size_t>(target_slot)]
                              .load(std::memory_order_relaxed);
        const auto d    = pdc_d_samples_[static_cast<std::size_t>(target_slot)]
                              .load(std::memory_order_relaxed);
        if (conf > best_conf) {
            best_conf      = conf;
            best_d_samples = d;
        }

        // Early exit on confident measurement.
        if (conf >= kConfidentEnough) return true;

        // Otherwise keep going up to the cap, then accept the best.
        if (measurement_count >= kMaxMeasurements) {
            // Make sure the best-so-far is the value the writer/UI sees,
            // not whatever the last (possibly worse) measurement was.
            pdc_d_samples_ [static_cast<std::size_t>(target_slot)]
                .store(best_d_samples, std::memory_order_relaxed);
            pdc_confidence_[static_cast<std::size_t>(target_slot)]
                .store(best_conf, std::memory_order_relaxed);
            return best_conf >= kConfidentEnough;
        }

        solo_cali_state_.store(static_cast<std::uint8_t>(SoloCaliState::Capturing),
                                std::memory_order_release);
        juce::Thread::getCurrentThread()->wait(kInterMeasureMs);
    }
}

// Cross-correlate each active sat's recent audio against the hub-input
// reference ring to estimate D. The reference ring is the DAW-PDC-aligned
// mix; sat rings carry pre-PDC content from each track. The peak lag of
// the cross-correlation IS the per-track pre-roll D — positive means sat's
// content is `D` samples ahead of what master heard at the same wall-clock.
void HubProcessor::pdcCalibratorTick() {
    pdc_tick_count_.fetch_add(1, std::memory_order_relaxed);
    if (region_ == nullptr) return;
    if (!pdc_ref_anchor_set_.load(std::memory_order_acquire)) return;

    constexpr int N = 1024;  // sat window length (~21ms @ 48k) — short enough to
                             // fit inside a short sampler-clip burst (~50ms+)
    constexpr int K = 4096;  // ± lag search range (~85ms @ 48k) — covers Bitwig's
                             // observed PDC offsets which can run to 40-85 ms
    constexpr double kMinEnergy      = 1e-6;

    const auto hub_anchor = pdc_ref_anchor_host_frame_.load(std::memory_order_relaxed);
    const auto cap        = static_cast<std::uint32_t>(pdc_ref_data_.size());
    const auto hub_mask   = cap - 1u;
    const auto hub_wp_now = pdc_ref_header_.write_pos.load(std::memory_order_acquire);
    if (hub_wp_now < static_cast<std::uint64_t>(N + 2 * K)) return;

    auto readHub = [&](std::int64_t start_wp, int len, std::vector<float>& out) {
        out.resize(static_cast<std::size_t>(len));
        for (int i = 0; i < len; ++i) {
            const auto idx = static_cast<std::uint32_t>(
                (static_cast<std::uint64_t>(start_wp + i)) & hub_mask);
            out[static_cast<std::size_t>(i)] = pdc_ref_data_[idx];
        }
    };

    std::vector<float> sat_interleaved(static_cast<std::size_t>(N) * RING_CHANNELS);
    std::vector<float> sat_mono(static_cast<std::size_t>(N));
    std::vector<float> hub_window;

    auto setSkip = [this](std::uint32_t slot, PdcSkip reason) {
        pdc_last_skip_[slot].store(static_cast<std::uint8_t>(reason), std::memory_order_relaxed);
    };

    for (std::uint32_t i = 0; i < gatherer::protocol::NUM_SLOTS; ++i) {
        if (region_->slots[i].state.load(std::memory_order_acquire) != SLOT_STATE_ACTIVE) {
            setSkip(i, PdcSkip::SlotInactive);
            continue;
        }

        SpscRingBuffer rb(region_->slots[i].ring_header,
                          region_->slots[i].ring_data,
                          RING_FRAMES, RING_CHANNELS);
        const auto sat_wp_now = rb.writePos();

        const auto sat_last_master =
            region_->slots[i].last_write_host_frame.load(std::memory_order_acquire);
        if (sat_last_master <= 0) { setSkip(i, PdcSkip::SatNotWritten); continue; }

        const auto safety = static_cast<std::uint64_t>(maxBlockSize() > 0
                                                       ? maxBlockSize() : 1024);
        const auto backshift = static_cast<std::uint64_t>(N + K) + safety;
        if (sat_wp_now < backshift) { setSkip(i, PdcSkip::SatNotEnoughData); continue; }
        const auto sat_start = sat_wp_now - backshift;
        if (!rb.peekAt(sat_start, sat_interleaved.data(), N)) {
            setSkip(i, PdcSkip::SatRingOverrun); continue;
        }
        for (int j = 0; j < N; ++j) {
            sat_mono[static_cast<std::size_t>(j)]
                = 0.5f * (sat_interleaved[j * 2] + sat_interleaved[j * 2 + 1]);
        }

        const std::int64_t baseline_start =
            static_cast<std::int64_t>(sat_last_master) - N - K
            - static_cast<std::int64_t>(safety) - hub_anchor;

        const std::int64_t hub_window_start = baseline_start - K;
        const int hub_window_len = N + 2 * K;
        if (hub_window_start < 0) {
            setSkip(i, PdcSkip::HubWindowBeforeZero); continue;
        }
        if (static_cast<std::uint64_t>(hub_window_start + hub_window_len) > hub_wp_now) {
            setSkip(i, PdcSkip::HubWindowPastWrite); continue;
        }
        if (hub_wp_now > static_cast<std::uint64_t>(hub_window_start)
            && hub_wp_now - static_cast<std::uint64_t>(hub_window_start) > cap) {
            setSkip(i, PdcSkip::HubWindowOutOfCap); continue;
        }

        readHub(hub_window_start, hub_window_len, hub_window);

        double norm_sat = 0.0;
        for (int j = 0; j < N; ++j) {
            const double s = sat_mono[static_cast<std::size_t>(j)];
            norm_sat += s * s;
        }
        if (norm_sat < kMinEnergy) { setSkip(i, PdcSkip::SatSilent); continue; }

        // Time-domain cross-correlation across lag k in [-K, K]. Sweep
        // and track the peak. We tried FFT GCC-PHAT — yielded uniformly
        // zero readings (the PHAT IFFT happened to peak right at L=K =
        // "best_k=0" for every track regardless of true alignment),
        // possibly because solo-cali's isolated content is *too* clean
        // and the PHAT-whitened spectrum has no spectral discrimination.
        // Time-domain XCorr empirically produced useful kick measurement
        // before, so we stay with it.
        double max_abs    = 0.0;
        int    best_k     = 0;
        for (int k = -K; k <= K; ++k) {
            const int hub_offset = K + k;
            double sum = 0.0;
            for (int j = 0; j < N; ++j) {
                sum += static_cast<double>(sat_mono[static_cast<std::size_t>(j)])
                     * static_cast<double>(hub_window[static_cast<std::size_t>(hub_offset + j)]);
            }
            const double absSum = std::abs(sum);
            if (absSum > max_abs) { max_abs = absSum; best_k = k; }
        }

        double norm_hub = 0.0;
        for (int j = 0; j < N; ++j) {
            const double h = hub_window[static_cast<std::size_t>(K + best_k + j)];
            norm_hub += h * h;
        }
        if (norm_hub < kMinEnergy) { setSkip(i, PdcSkip::HubSilent); continue; }

        const double normalized = max_abs / std::sqrt(norm_sat * norm_hub);
        pdc_confidence_[i].store(static_cast<float>(normalized), std::memory_order_relaxed);

        pdc_d_samples_[i].store(static_cast<std::int64_t>(best_k),
                                 std::memory_order_relaxed);
        pdc_success_count_.fetch_add(1, std::memory_order_relaxed);
        setSkip(i, PdcSkip::Ok);
    }
}

bool HubProcessor::isBusesLayoutSupported(const BusesLayout& layouts) const {
    return layouts.getMainOutputChannelSet() == juce::AudioChannelSet::stereo()
        && (layouts.getMainInputChannelSet() == juce::AudioChannelSet::stereo()
            || layouts.getMainInputChannelSet() == juce::AudioChannelSet::disabled());
}

void HubProcessor::processBlock(juce::AudioBuffer<float>& buffer, juce::MidiBuffer&) {
    juce::ScopedNoDenormals noDenormals;
    const int frames = buffer.getNumSamples();

    // PDC reference capture. Hub's input *is* the parent-bus mix as the DAW
    // hands it to us — by which point the DAW has already applied PDC across
    // every child track. That makes it a sample-accurate reference for
    // "music at master time T". Push a mono-summed copy into the ref ring
    // before we touch the buffer.
    //
    // Critical: write to the ref ring ONLY when transport is playing, and
    // anchor wp=0 to the master frame at the first playing block. This
    // makes (hub_anchor + hub_wp) == master frame at the latest written
    // sample, which the cross-correlator relies on to find sat content
    // in hub's ring. If we wrote on every callback regardless of playing
    // state, hub_wp would track wall-clock callbacks rather than master
    // time, and any data written while transport was stopped would push
    // the corresponding hub window past the ref ring's capacity.
    if (!pdc_ref_data_.empty() && frames > 0) {
        bool playing_now = false;
        std::int64_t hfs_at_block_start = 0;
        bool have_hfs = false;
        if (auto* ph = getPlayHead()) {
            if (auto pos = ph->getPosition()) {
                playing_now = pos->getIsPlaying();
                if (auto t = pos->getTimeInSamples()) {
                    hfs_at_block_start = *t;
                    have_hfs = true;
                }
            }
        }
        if (playing_now && have_hfs) {
            const auto* L = buffer.getReadPointer(0);
            const auto* R = buffer.getNumChannels() > 1 ? buffer.getReadPointer(1) : L;
            const auto cap  = static_cast<std::uint32_t>(pdc_ref_data_.size());
            const auto mask = cap - 1u;
            const auto wp_before = pdc_ref_header_.write_pos.load(std::memory_order_relaxed);
            for (int i = 0; i < frames; ++i) {
                const auto idx = static_cast<std::uint32_t>((wp_before + static_cast<std::uint64_t>(i)) & mask);
                pdc_ref_data_[idx] = 0.5f * (L[i] + R[i]);
            }
            pdc_ref_header_.write_pos.store(wp_before + static_cast<std::uint64_t>(frames),
                                             std::memory_order_release);

            if (!pdc_ref_anchor_set_.load(std::memory_order_relaxed)) {
                pdc_ref_anchor_host_frame_.store(hfs_at_block_start, std::memory_order_relaxed);
                pdc_ref_anchor_set_.store(true, std::memory_order_release);
            }
        }
    }

    // Input handling — see isIncludeTrackInput() doc.
    //   OFF: discard whatever the host gave us; output is purely the sat-ring mix.
    //   ON : keep the input buffer as a source; sum sat-ring contributions on top.
    const bool keep_input = include_track_input_.load(std::memory_order_relaxed);
    if (!keep_input) buffer.clear();

    if (region_ == nullptr || !is_hub_) return;

    region_->header.hub_heartbeat.fetch_add(1, std::memory_order_relaxed);

    // Stash the host's transport play-state for the message thread (Record
    // button handler reads it to decide immediate-vs-deferred start).
    // If the user pre-armed a record while transport was stopped and
    // transport has now flipped to playing, post a one-shot to the message
    // thread so actuallyStartRecording happens there (writers / files are
    // not RT-safe to create on the audio thread).
    {
        bool playing_now = false;
        if (auto* ph = getPlayHead()) {
            if (auto pos = ph->getPosition()) {
                playing_now = pos->getIsPlaying();
            }
        }
        last_seen_playing_.store(playing_now, std::memory_order_release);

        if (playing_now
            && armed_pending_.load(std::memory_order_acquire)
            && !play_trigger_posted_.exchange(true, std::memory_order_acq_rel)) {
            // Race-free wp snapshot, all armed slots in one audio callback.
            // The anchor is wp_at_start_of_THIS_block (= prev_slot_state_[i].wp,
            // updated at the end of the previous processBlock) so recording
            // sample 0 corresponds to the first sample of the play-start
            // block — not the next one, which is what sat.wp_now after sat's
            // upstream write would give us.
            //
            // For pad-ON we also capture per-slot start_in_beats here, set to
            // now_beats (block start). The delta-based formula used by the
            // grid-capture loop later would give F_block_end for any slot
            // with delta == 0 (a clip-gated sat that didn't write this block),
            // which would offset the WAV's timeline by one block.
            const bool pad_at_snapshot = pad_silence_in_record_.load(std::memory_order_relaxed);
            double snap_now_seconds = 0.0;
            double snap_now_beats   = 0.0;
            if (auto* ph = getPlayHead()) {
                if (auto pos_in = ph->getPosition()) {
                    if (auto s = pos_in->getTimeInSeconds()) snap_now_seconds = *s;
                    if (auto p = pos_in->getPpqPosition())   snap_now_beats   = *p;
                }
            }
            for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
                if (region_->slots[i].state.load(std::memory_order_acquire) != SLOT_STATE_ACTIVE) continue;
                if (!mix_[i].record_arm.load(std::memory_order_relaxed)) continue;

                // If the sat occupying this slot has changed since we last
                // saw it (mid-session reclaim), prev_slot_state_[i].wp refers
                // to the previous sat's ring and is stale — fall back to the
                // sat's current wp.
                const auto cur_uuid = region_->slots[i].sat_uuid.load(std::memory_order_acquire);
                std::uint64_t wp_anchor = prev_slot_state_[i].wp;
                if (cur_uuid != prev_slot_state_[i].uuid) {
                    wp_anchor = region_->slots[i].ring_header.write_pos
                                    .load(std::memory_order_acquire);
                }

                recording_start_wp_[i].store(wp_anchor, std::memory_order_release);
                recording_active_  [i].store(true, std::memory_order_release);
                expected_samples_  [i].store(0, std::memory_order_release);

                auto& ps = grid_.per_slot[i];
                if (pad_at_snapshot) {
                    ps.start_in_seconds.store(snap_now_seconds, std::memory_order_relaxed);
                    ps.start_in_beats  .store(snap_now_beats,   std::memory_order_relaxed);
                    ps.captured        .store(true, std::memory_order_release);
                } else {
                    ps.captured.store(false, std::memory_order_release);
                }
            }
            juce::MessageManager::callAsync([this] {
                play_trigger_posted_.store(false, std::memory_order_release);
                if (!armed_pending_.exchange(false, std::memory_order_acq_rel)) return;
                actuallyStartRecording();
            });
        }
    }

    // Grid capture — runs only while recording AND transport is playing.
    //
    // Session-level (bpm + time sig) is captured once per session.
    //
    // Per-slot start_in_seconds/beats is captured for each individual recording
    // (so re-recording a single track from a new DAW position keeps that lane's
    // grid honest, independent of any earlier recordings in the same session).
    //
    // Sample-accurate back-correction:
    //   playhead.getTimeInSamples() is the frame at the START of this block.
    //   sat is upstream and has already written this block, so sat.wp_now =
    //   sat.wp_at_block_start + frames. delta = wp_now − recording_start_wp.
    //   Recording sample-0 is at wp=recording_start_wp, which corresponds to
    //   DAW frame (block_end_frame − delta) = (playhead + frames) − delta.
    if (recorder_ && recorder_->isRecording()) {
        if (auto* ph = getPlayHead()) {
            if (auto pos = ph->getPosition()) {
                if (pos->getIsPlaying()) {
                    // Session-level once.
                    if (!grid_.captured.load(std::memory_order_acquire)) {
                        if (auto bpm = pos->getBpm())           grid_.bpm = *bpm;
                        if (auto ts  = pos->getTimeSignature()) {
                            grid_.time_sig_num = ts->numerator;
                            grid_.time_sig_den = ts->denominator;
                        }
                        grid_.captured.store(true, std::memory_order_release);
                    }

                    double now_seconds = 0.0, now_beats = 0.0;
                    if (auto s = pos->getTimeInSeconds()) now_seconds = *s;
                    if (auto p = pos->getPpqPosition())   now_beats   = *p;

                    const double sr            = getSampleRate();
                    const double block_dur_s   = (sr > 0.0) ? (frames / sr) : 0.0;
                    const double block_dur_b   = block_dur_s * grid_.bpm / 60.0;

                    // Per-slot capture + expected-samples accounting.
                    //
                    // When pad-silence is ON (default), all armed slots
                    // capture immediately on the snapshot block so every
                    // slot's start_in_beats == play-start; the writer pads
                    // zeros for any sat whose clip-gated host hasn't kicked
                    // off processBlock yet, so the WAV's sample 0 also lives
                    // at play-start. When OFF we wait for delta > 0 so the
                    // anchor is "first DAW frame this sat actually wrote"
                    // — produces tighter files but lanes won't share x=0.
                    //
                    // expected_samples_[i] grows by `frames` every active
                    // block regardless of toggle — the writer only consults
                    // it when pad-silence is ON, so OFF mode pays nothing.
                    const bool pad = pad_silence_in_record_.load(std::memory_order_relaxed);
                    for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
                        if (!recording_active_[i].load(std::memory_order_acquire)) continue;
                        if (region_->slots[i].state.load(std::memory_order_acquire) != SLOT_STATE_ACTIVE) continue;

                        expected_samples_[i].fetch_add(static_cast<std::uint64_t>(frames),
                                                       std::memory_order_release);

                        auto& ps = grid_.per_slot[i];
                        if (ps.captured.load(std::memory_order_acquire)) continue;

                        const auto start_wp = recording_start_wp_[i].load(std::memory_order_acquire);
                        const auto sat_wp   = region_->slots[i].ring_header.write_pos
                                                  .load(std::memory_order_acquire);
                        const std::int64_t delta = static_cast<std::int64_t>(sat_wp)
                                                 - static_cast<std::int64_t>(start_wp);
                        if (!pad && delta <= 0) continue;  // pad-off: wait for first write

                        const std::int64_t delta_for_calc = (delta > 0) ? delta : 0;
                        const double delta_s = (sr > 0.0)
                            ? static_cast<double>(delta_for_calc) / sr : 0.0;
                        const double delta_b = delta_s * grid_.bpm / 60.0;

                        ps.start_in_seconds.store(now_seconds + block_dur_s - delta_s,
                                                  std::memory_order_relaxed);
                        ps.start_in_beats  .store(now_beats   + block_dur_b - delta_b,
                                                  std::memory_order_relaxed);
                        ps.captured.store(true, std::memory_order_release);
                    }
                }
            }
        }
    }

    const auto target_lag = static_cast<std::uint64_t>(max_block_size_);

    // Mixer mode: if any active slot has solo on, only soloed slots contribute.
    bool any_solo_active = false;
    for (std::uint32_t s = 0; s < NUM_SLOTS; ++s) {
        if (region_->slots[s].state.load(std::memory_order_acquire) != SLOT_STATE_ACTIVE) continue;
        if (mix_[s].solo.load(std::memory_order_relaxed)) { any_solo_active = true; break; }
    }

    auto* L = buffer.getWritePointer(0);
    auto* R = buffer.getNumChannels() > 1 ? buffer.getWritePointer(1) : nullptr;

    const bool transport_playing = playback_.isPlaying();

    // Per-callback re-anchored peek. We don't cache a read position — instead we always
    // read at (current wp - current target_lag). Benefits:
    //   - Immune to prepareToPlay reporting a transient large samplesPerBlock at startup
    //     (which used to leave us drifting at a stale lag forever).
    //   - peekAt doesn't touch the shared read_pos, so no race with satellite's overrun.
    //   - When hub is double-called (wp unchanged), we skip the read instead of duplicating
    //     the previous output block (which was the source of transient artifacts).
    for (std::uint32_t s = 0; s < NUM_SLOTS; ++s) {
        auto& slot = region_->slots[s];
        if (slot.state.load(std::memory_order_acquire) != SLOT_STATE_ACTIVE) continue;

        // Source selection: when transport is Playing AND this slot has a loaded
        // playback source, read from the file. Otherwise fall back to live sat
        // audio. This lets the user review recordings while uninstalled sats
        // continue playing live.
        bool have_data = false;
        if (transport_playing && playback_.hasSourceForSlot(static_cast<int>(s))) {
            have_data = playback_.readSlotIntoInterleaved(
                static_cast<int>(s), scratch_.data(), frames);
        }
        if (!have_data) {
            SpscRingBuffer rb(slot.ring_header, slot.ring_data, RING_FRAMES, RING_CHANNELS);
            const auto wp   = rb.writePos();
            const auto uuid = slot.sat_uuid.load(std::memory_order_acquire);
            auto& state     = slot_states_[s];

            if (state.last_uuid != uuid) {
                state.last_uuid    = uuid;
                state.last_seen_wp = 0;
            }

            if (wp == state.last_seen_wp) continue;
            state.last_seen_wp = wp;

            if (wp < target_lag) continue;

            if (!rb.peekAt(wp - target_lag,
                           scratch_.data(),
                           static_cast<std::uint32_t>(frames))) continue;
        }

        // (LUFS feeding/querying now lives on the LufsWorker thread — see
        // lufsWorkerTick. The audio thread no longer touches the analyzer.)

        // Mix gating: a slot contributes to the master mix unless it's muted,
        // or another slot is soloed and this one isn't.
        const bool  muted       = mix_[s].mute    .load(std::memory_order_relaxed);
        const bool  this_solo   = mix_[s].solo    .load(std::memory_order_relaxed);
        const float vol_gain    = mix_[s].gain_lin.load(std::memory_order_relaxed);
        const float norm_gain   = mix_[s].norm_lin.load(std::memory_order_relaxed);
        const float gain        = vol_gain * norm_gain;
        const bool  contributes = !muted && (!any_solo_active || this_solo);

        // Pre-fader L/R metering — reflects what the sat is producing, irrespective
        // of gain/mute/solo. The user can see incoming activity even on a muted layer.
        float  block_peak_l = 0.0f, block_peak_r = 0.0f;
        double block_sumsq_l = 0.0, block_sumsq_r = 0.0;
        for (int i = 0; i < frames; ++i) {
            const float l = scratch_[static_cast<std::size_t>(i) * RING_CHANNELS + 0];
            const float r = scratch_[static_cast<std::size_t>(i) * RING_CHANNELS + 1];
            block_peak_l = std::max(block_peak_l, std::abs(l));
            block_peak_r = std::max(block_peak_r, std::abs(r));
            block_sumsq_l += static_cast<double>(l) * l;
            block_sumsq_r += static_cast<double>(r) * r;
            if (contributes) {
                L[i] += l * gain;
                if (R) R[i] += r * gain;
            }
        }
        const float block_rms_l = std::sqrt(static_cast<float>(
            block_sumsq_l / static_cast<double>(frames)));
        const float block_rms_r = std::sqrt(static_cast<float>(
            block_sumsq_r / static_cast<double>(frames)));

        auto& lvl = levels_[s];
        // Peak ballistics: instant attack, ~10 dB/sec decay.
        const float prev_pl = lvl.peak_lin_l.load(std::memory_order_relaxed);
        const float prev_pr = lvl.peak_lin_r.load(std::memory_order_relaxed);
        lvl.peak_lin_l.store(std::max(prev_pl * 0.97f, block_peak_l),
                             std::memory_order_relaxed);
        lvl.peak_lin_r.store(std::max(prev_pr * 0.97f, block_peak_r),
                             std::memory_order_relaxed);
        const float prev_rl = lvl.rms_lin_l.load(std::memory_order_relaxed);
        const float prev_rr = lvl.rms_lin_r.load(std::memory_order_relaxed);
        lvl.rms_lin_l.store(prev_rl * 0.7f + block_rms_l * 0.3f,
                            std::memory_order_relaxed);
        lvl.rms_lin_r.store(prev_rr * 0.7f + block_rms_r * 0.3f,
                            std::memory_order_relaxed);
    }

    if (transport_playing) playback_.advancePlayhead(frames);

    // Update per-slot snapshots for next block's potential rising-edge use.
    // Stores wp_after_this_block, which equals wp_at_start_of_next_block.
    // Also tracks sat uuid so a reclaim between blocks invalidates the wp.
    for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
        const auto& sat = region_->slots[i];
        prev_slot_state_[i].wp   = sat.ring_header.write_pos.load(std::memory_order_acquire);
        prev_slot_state_[i].uuid = sat.sat_uuid.load(std::memory_order_acquire);
    }
}

juce::AudioProcessorEditor* HubProcessor::createEditor() {
    return new HubEditor(*this);
}

void HubProcessor::getStateInformation(juce::MemoryBlock& destData) {
    juce::ValueTree tree("GathererHub");
    tree.setProperty("uuid_hi", static_cast<juce::int64>(my_uuid_ >> 32), nullptr);
    tree.setProperty("uuid_lo", static_cast<juce::int64>(my_uuid_ & 0xFFFFFFFFull), nullptr);
    tree.setProperty("include_track_input",
                     include_track_input_.load(std::memory_order_relaxed), nullptr);
    tree.setProperty("target_lufs",
                     target_lufs_.load(std::memory_order_relaxed), nullptr);
    tree.setProperty("pad_silence_in_record",
                     pad_silence_in_record_.load(std::memory_order_relaxed), nullptr);
    tree.setProperty("session_folder", session_.serializeForPluginState(), nullptr);

    juce::String display_order_str;
    for (std::size_t i = 0; i < display_order_.size(); ++i) {
        if (i > 0) display_order_str += ',';
        display_order_str += juce::String(display_order_[i]);
    }
    tree.setProperty("display_order", display_order_str, nullptr);

    juce::ValueTree mix("mix");
    for (std::size_t i = 0; i < mix_.size(); ++i) {
        juce::ValueTree slot("slot");
        slot.setProperty("i",    static_cast<int>(i), nullptr);
        slot.setProperty("mute", mix_[i].mute.load(std::memory_order_relaxed), nullptr);
        slot.setProperty("solo", mix_[i].solo.load(std::memory_order_relaxed), nullptr);
        slot.setProperty("gain_db",
                          linToDb(mix_[i].gain_lin.load(std::memory_order_relaxed)),
                          nullptr);
        slot.setProperty("norm_db",
                          linToDb(mix_[i].norm_lin.load(std::memory_order_relaxed)),
                          nullptr);
        slot.setProperty("target_lufs",
                          mix_[i].target_lufs.load(std::memory_order_relaxed),
                          nullptr);
        slot.setProperty("record_arm",
                          mix_[i].record_arm.load(std::memory_order_relaxed), nullptr);
        mix.appendChild(slot, nullptr);
    }
    tree.appendChild(mix, nullptr);

    juce::MemoryOutputStream stream(destData, false);
    tree.writeToStream(stream);
}

void HubProcessor::setStateInformation(const void* data, int sizeInBytes) {
    if (data == nullptr || sizeInBytes <= 0) return;
    // State restore is not itself undoable. Clear the stack so a freshly loaded
    // project doesn't start with bizarre cross-session undo entries.
    command_stack_.clear();
    juce::MemoryInputStream stream(data, static_cast<std::size_t>(sizeInBytes), false);
    auto tree = juce::ValueTree::readFromStream(stream);
    if (!tree.isValid() || tree.getType() != juce::Identifier("GathererHub")) return;

    if (tree.hasProperty("include_track_input")) {
        include_track_input_.store(static_cast<bool>(tree["include_track_input"]),
                                   std::memory_order_relaxed);
    }
    if (tree.hasProperty("target_lufs")) {
        target_lufs_.store(static_cast<float>(static_cast<double>(tree["target_lufs"])),
                           std::memory_order_relaxed);
    }
    if (tree.hasProperty("pad_silence_in_record")) {
        pad_silence_in_record_.store(static_cast<bool>(tree["pad_silence_in_record"]),
                                      std::memory_order_relaxed);
    }
    if (tree.hasProperty("session_folder")) {
        session_.restoreFromPluginState(tree["session_folder"].toString());
    }
    if (tree.hasProperty("display_order")) {
        const auto raw = tree["display_order"].toString();
        juce::StringArray parts;
        parts.addTokens(raw, ",", "");
        std::array<int, gatherer::protocol::NUM_SLOTS> order{};
        if (parts.size() == static_cast<int>(order.size())) {
            for (int i = 0; i < parts.size(); ++i) {
                order[static_cast<std::size_t>(i)] = parts[i].getIntValue();
            }
            setDisplayOrder(order);
        }
    }

    auto mix = tree.getChildWithName("mix");
    if (mix.isValid()) {
        for (int c = 0; c < mix.getNumChildren(); ++c) {
            auto slot = mix.getChild(c);
            const int i = slot.getProperty("i", -1);
            if (i < 0 || i >= static_cast<int>(mix_.size())) continue;
            mix_[i].mute.store(static_cast<bool>(slot.getProperty("mute", false)),
                                std::memory_order_relaxed);
            mix_[i].solo.store(static_cast<bool>(slot.getProperty("solo", false)),
                                std::memory_order_relaxed);
            const float db = static_cast<float>(static_cast<double>(slot.getProperty("gain_db", 0.0)));
            mix_[i].gain_lin.store(dbToLin(juce::jlimit(kGainDbMin, kGainDbMax, db)),
                                    std::memory_order_relaxed);
            const float norm_db = static_cast<float>(static_cast<double>(slot.getProperty("norm_db", 0.0)));
            mix_[i].norm_lin.store(dbToLin(juce::jlimit(kGainDbMin, kGainDbMax, norm_db)),
                                    std::memory_order_relaxed);
            const float t = static_cast<float>(static_cast<double>(slot.getProperty(
                "target_lufs", static_cast<double>(target_lufs_.load(std::memory_order_relaxed)))));
            mix_[i].target_lufs.store(juce::jlimit(-60.0f, 0.0f, t),
                                       std::memory_order_relaxed);
            mix_[i].record_arm.store(static_cast<bool>(slot.getProperty("record_arm", false)),
                                      std::memory_order_relaxed);
        }
    }
    // (UUID is not restored — we keep the freshly generated one so a project reload
    // doesn't fight over hub_uuid with another instance.)
}

int HubProcessor::activeSatellites() const noexcept {
    if (region_ == nullptr) return 0;
    int n = 0;
    for (const auto& slot : region_->slots) {
        if (slot.state.load(std::memory_order_acquire) == SLOT_STATE_ACTIVE) ++n;
    }
    return n;
}

std::array<int, gatherer::protocol::NUM_SLOTS> HubProcessor::getDisplayOrder() const noexcept {
    return display_order_;
}

void HubProcessor::setDisplayOrder(const std::array<int, gatherer::protocol::NUM_SLOTS>& order) noexcept {
    // Validate: every slot index 0..N-1 must appear exactly once.
    std::array<bool, gatherer::protocol::NUM_SLOTS> seen{};
    for (int v : order) {
        if (v < 0 || v >= static_cast<int>(gatherer::protocol::NUM_SLOTS)) return;
        if (seen[static_cast<std::size_t>(v)]) return;
        seen[static_cast<std::size_t>(v)] = true;
    }
    display_order_ = order;
}

void HubProcessor::moveSlotInDisplayOrder(int slot, int direction) noexcept {
    if (direction != -1 && direction != 1) return;
    int pos = -1;
    for (int i = 0; i < static_cast<int>(display_order_.size()); ++i) {
        if (display_order_[i] == slot) { pos = i; break; }
    }
    if (pos < 0) return;
    const int new_pos = pos + direction;
    if (new_pos < 0 || new_pos >= static_cast<int>(display_order_.size())) return;
    const int other_slot = display_order_[new_pos];
    std::swap(display_order_[pos], display_order_[new_pos]);

    // Position-bound mix params (mute / solo / volume gain) swap between the
    // two slots so each visual row keeps the values that lived at its position
    // before the reorder. Per-sat params (norm gain, target LUFS, record arm)
    // stay with the slot — they belong to the audio source, not the row.
    if (slot < 0 || slot >= static_cast<int>(mix_.size())) return;
    if (other_slot < 0 || other_slot >= static_cast<int>(mix_.size())) return;
    auto& a = mix_[slot];
    auto& b = mix_[other_slot];

    const auto swap_bool = [](std::atomic<bool>& x, std::atomic<bool>& y) {
        const bool xv = x.load(std::memory_order_relaxed);
        const bool yv = y.load(std::memory_order_relaxed);
        x.store(yv, std::memory_order_relaxed);
        y.store(xv, std::memory_order_relaxed);
    };
    const auto swap_float = [](std::atomic<float>& x, std::atomic<float>& y) {
        const float xv = x.load(std::memory_order_relaxed);
        const float yv = y.load(std::memory_order_relaxed);
        x.store(yv, std::memory_order_relaxed);
        y.store(xv, std::memory_order_relaxed);
    };
    swap_bool (a.mute,     b.mute);
    swap_bool (a.solo,     b.solo);
    swap_float(a.gain_lin, b.gain_lin);
}

std::vector<HubProcessor::SatelliteSnapshot> HubProcessor::snapshotSatellites() const {
    std::vector<SatelliteSnapshot> out;
    if (region_ == nullptr) return out;
    for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
        const auto& slot = region_->slots[i];
        if (slot.state.load(std::memory_order_acquire) != SLOT_STATE_ACTIVE) continue;
        SatelliteSnapshot snap;
        snap.slot_index            = static_cast<int>(i);
        snap.uuid                  = slot.sat_uuid.load(std::memory_order_acquire);
        snap.display_name          = juce::String::fromUTF8(slot.display_name);
        snap.track_name            = juce::String::fromUTF8(slot.track_name);
        snap.heartbeat             = static_cast<std::uint32_t>(slot.sat_heartbeat.load(std::memory_order_relaxed));
        snap.write_pos             = slot.ring_header.write_pos.load(std::memory_order_acquire);
        snap.last_write_host_frame = slot.last_write_host_frame.load(std::memory_order_relaxed);
        snap.color_rgba            = slot.color_rgba;
        out.push_back(std::move(snap));
    }
    return out;
}

void HubProcessor::reclaimGhostSlots() {
    if (region_ == nullptr) return;

    for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
        auto& slot = region_->slots[i];
        if (slot.state.load(std::memory_order_acquire) != SLOT_STATE_ACTIVE) continue;

        const auto pid = slot.sat_pid.load(std::memory_order_acquire);
        if (isPidAlive(pid)) continue;  // sat process is alive — leave it alone

        // Ghost: the owning process no longer exists (host exited without
        // destructing the plugin, crash, etc.). Reset the slot so a fresh
        // claim can take it.
        slot.sat_uuid.store(0, std::memory_order_release);
        slot.sat_pid.store(0, std::memory_order_release);
        slot.state.store(SLOT_STATE_EMPTY, std::memory_order_release);

        slot_states_[i] = {};
        auto& lvl = levels_[i];
        lvl.peak_lin_l.store(0.0f, std::memory_order_relaxed);
        lvl.peak_lin_r.store(0.0f, std::memory_order_relaxed);
        lvl.rms_lin_l .store(0.0f, std::memory_order_relaxed);
        lvl.rms_lin_r .store(0.0f, std::memory_order_relaxed);
        // LUFS state: only touch the atomics here (worker-thread state stays
        // worker-owned). The worker detects the UUID transition on its next
        // tick and resets its analyzer + last_fed_wp itself.
        lufs_[i].integrated.store(-100.0f, std::memory_order_relaxed);
        lufs_[i].momentary .store(-100.0f, std::memory_order_relaxed);
        lufs_[i].short_term.store(-100.0f, std::memory_order_relaxed);
    }
}

std::uint64_t HubProcessor::hubHeartbeat() const noexcept {
    if (region_ == nullptr) return 0;
    return region_->header.hub_heartbeat.load(std::memory_order_relaxed);
}

std::uint32_t HubProcessor::maxBlockSize() const noexcept {
    if (region_ == nullptr) return static_cast<std::uint32_t>(max_block_size_);
    return region_->header.max_block_size.load(std::memory_order_relaxed);
}

HubProcessor::LevelSnapshot HubProcessor::getSlotLevels(int slot_index) const noexcept {
    LevelSnapshot s { -100.0f, -100.0f, -100.0f, -100.0f };
    if (slot_index < 0 || slot_index >= static_cast<int>(levels_.size())) return s;
    const auto& lvl = levels_[slot_index];

    const float pl = lvl.peak_lin_l.load(std::memory_order_relaxed);
    const float pr = lvl.peak_lin_r.load(std::memory_order_relaxed);
    const float rl = lvl.rms_lin_l .load(std::memory_order_relaxed);
    const float rr = lvl.rms_lin_r .load(std::memory_order_relaxed);

    s.peak_db_l = (pl > 1e-7f) ? 20.0f * std::log10(pl) : -100.0f;
    s.peak_db_r = (pr > 1e-7f) ? 20.0f * std::log10(pr) : -100.0f;
    s.rms_db_l  = (rl > 1e-7f) ? 20.0f * std::log10(rl) : -100.0f;
    s.rms_db_r  = (rr > 1e-7f) ? 20.0f * std::log10(rr) : -100.0f;
    return s;
}

bool HubProcessor::normalizeSlotGainToTarget(int slot_index) noexcept {
    if (slot_index < 0 || slot_index >= static_cast<int>(lufs_.size())) return false;
    const float integrated = lufs_[slot_index].integrated.load(std::memory_order_relaxed);
    if (integrated <= -99.0f) return false;
    // Per-slot target wins; falls back to global default at row creation time.
    const float target = mix_[slot_index].target_lufs.load(std::memory_order_relaxed);
    // LUFS scales 1:1 with dB gain on the source signal. Pre-fader integrated of
    // -18 LUFS + gain of +4 dB → post-fader contribution of -14 LUFS.
    setNormalizeDb(slot_index, target - integrated);
    return true;
}

void HubProcessor::normalizeAllActiveSlots() noexcept {
    if (region_ == nullptr) return;
    for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
        if (region_->slots[i].state.load(std::memory_order_acquire) != SLOT_STATE_ACTIVE) continue;
        normalizeSlotGainToTarget(static_cast<int>(i));
    }
}

float HubProcessor::getNormalizeDb(int slot) const noexcept {
    if (slot < 0 || slot >= static_cast<int>(mix_.size())) return 0.0f;
    return linToDb(mix_[slot].norm_lin.load(std::memory_order_relaxed));
}
void HubProcessor::setNormalizeDb(int slot, float db) noexcept {
    if (slot < 0 || slot >= static_cast<int>(mix_.size())) return;
    db = juce::jlimit(kGainDbMin, kGainDbMax, db);
    mix_[slot].norm_lin.store(dbToLin(db), std::memory_order_relaxed);
}

float HubProcessor::getSlotTargetLufs(int slot) const noexcept {
    if (slot < 0 || slot >= static_cast<int>(mix_.size())) return target_lufs_.load(std::memory_order_relaxed);
    return mix_[slot].target_lufs.load(std::memory_order_relaxed);
}
void HubProcessor::setSlotTargetLufs(int slot, float v) noexcept {
    if (slot < 0 || slot >= static_cast<int>(mix_.size())) return;
    mix_[slot].target_lufs.store(juce::jlimit(-60.0f, 0.0f, v), std::memory_order_relaxed);
}

HubProcessor::LufsSnapshot HubProcessor::getSlotLufs(int slot_index) const noexcept {
    LufsSnapshot s { -100.0f, -100.0f, -100.0f };
    if (slot_index < 0 || slot_index >= static_cast<int>(lufs_.size())) return s;
    const auto& sl = lufs_[slot_index];
    s.integrated = sl.integrated .load(std::memory_order_relaxed);
    s.momentary  = sl.momentary  .load(std::memory_order_relaxed);
    s.short_term = sl.short_term .load(std::memory_order_relaxed);
    return s;
}

juce::AudioThumbnail* HubProcessor::getThumbnail(int slot_index) const noexcept {
    if (slot_index < 0 || slot_index >= static_cast<int>(thumbnails_.size())) return nullptr;
    return thumbnails_[slot_index].get();
}

bool HubProcessor::getMute(int slot) const noexcept {
    if (slot < 0 || slot >= static_cast<int>(mix_.size())) return false;
    return mix_[slot].mute.load(std::memory_order_relaxed);
}
void HubProcessor::setMute(int slot, bool on) noexcept {
    if (slot < 0 || slot >= static_cast<int>(mix_.size())) return;
    mix_[slot].mute.store(on, std::memory_order_relaxed);
}
bool HubProcessor::getSolo(int slot) const noexcept {
    if (slot < 0 || slot >= static_cast<int>(mix_.size())) return false;
    return mix_[slot].solo.load(std::memory_order_relaxed);
}
void HubProcessor::setSolo(int slot, bool on) noexcept {
    if (slot < 0 || slot >= static_cast<int>(mix_.size())) return;
    mix_[slot].solo.store(on, std::memory_order_relaxed);
}
float HubProcessor::getGainDb(int slot) const noexcept {
    if (slot < 0 || slot >= static_cast<int>(mix_.size())) return 0.0f;
    return linToDb(mix_[slot].gain_lin.load(std::memory_order_relaxed));
}
void HubProcessor::setGainDb(int slot, float db) noexcept {
    if (slot < 0 || slot >= static_cast<int>(mix_.size())) return;
    db = juce::jlimit(kGainDbMin, kGainDbMax, db);
    mix_[slot].gain_lin.store(dbToLin(db), std::memory_order_relaxed);
}

bool HubProcessor::getRecordArm(int slot) const noexcept {
    if (slot < 0 || slot >= static_cast<int>(mix_.size())) return false;
    return mix_[slot].record_arm.load(std::memory_order_relaxed);
}
void HubProcessor::setRecordArm(int slot, bool on) noexcept {
    if (slot < 0 || slot >= static_cast<int>(mix_.size())) return;
    mix_[slot].record_arm.store(on, std::memory_order_relaxed);
}

bool HubProcessor::startRecording() {
    if (region_ == nullptr) return false;
    if (recorder_ && recorder_->isRecording()) return false;
    if (armed_pending_.load(std::memory_order_acquire))  return false;

    // Verify something is actually armed (otherwise the audio thread would
    // do nothing on the next play block and the UI would look stuck).
    bool any_armed = false;
    for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
        if (region_->slots[i].state.load(std::memory_order_acquire) != SLOT_STATE_ACTIVE) continue;
        if (!mix_[i].record_arm.load(std::memory_order_relaxed)) continue;
        any_armed = true;
        break;
    }
    if (!any_armed) return false;

    // The audio thread will see armed_pending_ + isPlaying on the next block,
    // snapshot every armed slot's sat.wp in one race-free pass, and post a
    // callAsync that creates writers from those snapshotted values. This
    // unifies "DAW playing already" and "DAW paused" into a single deferred
    // path so the snapshot loop never interleaves with sat audio-thread writes.
    armed_pending_.store(true, std::memory_order_release);
    play_trigger_posted_.store(false, std::memory_order_release);
    return true;
}

bool HubProcessor::actuallyStartRecording() {
    // Audio thread has already populated recording_active_ and
    // recording_start_wp_ for every slot that was armed at the moment of
    // play-detection. Just turn that into an ArmedLayer list.
    const bool pad = pad_silence_in_record_.load(std::memory_order_relaxed);
    std::vector<Recorder::ArmedLayer> armed;
    for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
        if (!recording_active_[i].load(std::memory_order_acquire)) continue;
        if (region_->slots[i].state.load(std::memory_order_acquire) != SLOT_STATE_ACTIVE) continue;
        Recorder::ArmedLayer a;
        a.slot             = static_cast<int>(i);
        a.track_name       = juce::String::fromUTF8(region_->slots[i].track_name);
        a.display_name     = juce::String::fromUTF8(region_->slots[i].display_name);
        a.thumbnail        = thumbnails_[i].get();

        // Apply per-sat PDC compensation. The cross-correlator measured D
        // such that sat's content at sat_wp X represents music master_at_X
        // shifted by D in music time. To make the WAV's frame F hold music
        // (recording_start + F), the writer must read sat at sat_wp =
        // snap_wp - D (signed). D > 0 → read earlier (sat is ahead, e.g.
        // pre-roll); D < 0 → read later (sat is behind, need to wait for
        // sat to catch up).
        const auto snap_wp = recording_start_wp_[i].load(std::memory_order_acquire);
        const auto d_samples = pdcDEffective(static_cast<int>(i));
        const std::int64_t snap_signed = static_cast<std::int64_t>(snap_wp);
        std::int64_t shifted = snap_signed - d_samples;
        // Clamp to valid ring positions:
        //   * never below 0
        //   * never more than RING_FRAMES-1 *behind* snap_wp (we can't read
        //     older data than the ring holds)
        const std::int64_t min_allowed = std::max<std::int64_t>(
            0, snap_signed - (static_cast<std::int64_t>(gatherer::protocol::RING_FRAMES) - 1));
        if (shifted < min_allowed) shifted = min_allowed;
        a.start_wp         = static_cast<std::uint64_t>(shifted);
        a.expected_samples = pad ? &expected_samples_[i] : nullptr;
        armed.push_back(a);
    }
    if (armed.empty()) return false;
    if (!recorder_) recorder_ = std::make_unique<Recorder>(region_);
    const auto folder = session_.ensureFolderForRecording();
    return recorder_->start(armed, getSampleRate(), folder);
}

void HubProcessor::stopRecording() {
    // Cancel a pending armed-but-not-yet-recording state if the user backs out
    // before the DAW transport ever starts (or before the message-thread
    // callAsync that creates writers has run). The audio thread may already
    // have flipped recording_active_ flags during its snapshot pass — clear
    // them so the next startRecording starts from a clean slate.
    if (armed_pending_.exchange(false, std::memory_order_acq_rel)) {
        for (auto& f : recording_active_) f.store(false, std::memory_order_release);
        play_trigger_posted_.store(false, std::memory_order_release);
        return;
    }

    if (!recorder_ || !recorder_->isRecording()) return;

    for (auto& f : recording_active_) f.store(false, std::memory_order_release);

    // Snapshot the file list BEFORE stop() clears writers_, then drain + finalize.
    std::vector<juce::File> recorded;
    for (const auto& s : recorder_->writerStatuses()) {
        if (s.file.existsAsFile()) recorded.push_back(s.file);
        // Remember per-slot so the "Delete recording" action can find it later.
        if (s.slot >= 0 && s.slot < static_cast<int>(last_recordings_.size())) {
            last_recordings_[s.slot] = s.file;
        }
    }
    recorder_->stop();

    // Load the just-recorded WAVs as playback sources so the user can review.
    refreshPlaybackSources();
    recomputeSessionLayout();
    session_.autoSave();
    // Normalized WAV files are no longer auto-rendered here — see exportNormalized().
}

bool HubProcessor::exportNormalized() {
    return launchAlignedExport(/*apply_normalize=*/true,
                                /*suffix=*/"_normalized");
}

bool HubProcessor::exportAligned() {
    return launchAlignedExport(/*apply_normalize=*/false,
                                /*suffix=*/"_aligned");
}

bool HubProcessor::launchAlignedExport(bool apply_normalize, const juce::String& suffix) {
    // Compute the session timeline span = max(offset + length) across slots
    // that have a recording on disk.
    std::int64_t session_length = 0;
    for (std::size_t i = 0; i < last_recordings_.size(); ++i) {
        if (last_recordings_[i] == juce::File{} || !last_recordings_[i].existsAsFile()) continue;
        const auto off = playback_.slotOffsetSamples(static_cast<int>(i));
        const auto len = playback_.slotLengthSamples(static_cast<int>(i));
        if (len > 0) session_length = std::max(session_length, off + len);
    }
    if (session_length == 0) return false;

    std::vector<OfflineNormalizer::Task> tasks;
    for (std::size_t i = 0; i < last_recordings_.size(); ++i) {
        const auto& f = last_recordings_[i];
        if (f == juce::File{} || !f.existsAsFile()) continue;

        OfflineNormalizer::Task t;
        t.file                  = f;
        t.gain_db               = apply_normalize ? getNormalizeDb(static_cast<int>(i)) : 0.0f;
        t.output_suffix         = suffix;
        t.offset_samples        = playback_.slotOffsetSamples(static_cast<int>(i));
        t.total_length_samples  = session_length;
        tasks.push_back(std::move(t));
    }
    if (tasks.empty()) return false;

    normalizer_.reset();  // join previous thread if any
    normalizer_ = std::make_unique<OfflineNormalizer>(std::move(tasks));
    normalizer_->startAsync();
    return true;
}

bool HubProcessor::isRecording() const noexcept {
    return recorder_ && recorder_->isRecording();
}

juce::File HubProcessor::currentRecordingFolder() const {
    return recorder_ ? recorder_->currentSessionFolder() : juce::File{};
}

HubProcessor::GridInfo HubProcessor::getCurrentGridInfo() const noexcept {
    GridInfo out;
    out.captured = grid_.captured.load(std::memory_order_acquire);
    if (!out.captured) return out;
    out.bpm          = grid_.bpm;
    out.time_sig_num = grid_.time_sig_num;
    out.time_sig_den = grid_.time_sig_den;
    // Reference start = first slot that captured (kept for compat with code
    // paths that don't yet thread per-slot info).
    for (const auto& ps : grid_.per_slot) {
        if (ps.captured.load(std::memory_order_acquire)) {
            out.start_in_seconds = ps.start_in_seconds.load(std::memory_order_relaxed);
            out.start_in_beats   = ps.start_in_beats  .load(std::memory_order_relaxed);
            break;
        }
    }
    return out;
}

void HubProcessor::setCurrentGridInfo(const GridInfo& g) noexcept {
    grid_.captured.store(false, std::memory_order_relaxed);
    grid_.bpm          = g.bpm;
    grid_.time_sig_num = g.time_sig_num;
    grid_.time_sig_den = g.time_sig_den;
    grid_.captured.store(g.captured, std::memory_order_release);
}

HubProcessor::SlotGridInfo HubProcessor::getSlotGridInfo(int slot) const noexcept {
    SlotGridInfo out;
    if (slot < 0 || slot >= static_cast<int>(grid_.per_slot.size())) return out;
    const auto& ps = grid_.per_slot[slot];
    out.captured = ps.captured.load(std::memory_order_acquire);
    if (!out.captured) return out;
    out.start_in_seconds = ps.start_in_seconds.load(std::memory_order_relaxed);
    out.start_in_beats   = ps.start_in_beats  .load(std::memory_order_relaxed);
    return out;
}

void HubProcessor::setSlotGridInfo(int slot, const SlotGridInfo& g) noexcept {
    if (slot < 0 || slot >= static_cast<int>(grid_.per_slot.size())) return;
    auto& ps = grid_.per_slot[slot];
    ps.captured.store(false, std::memory_order_relaxed);
    ps.start_in_seconds.store(g.start_in_seconds, std::memory_order_relaxed);
    ps.start_in_beats  .store(g.start_in_beats,   std::memory_order_relaxed);
    ps.captured.store(g.captured, std::memory_order_release);
}

void HubProcessor::resetGrid() noexcept {
    grid_.captured.store(false, std::memory_order_release);
    for (auto& ps : grid_.per_slot) {
        ps.captured.store(false, std::memory_order_release);
    }
}

double HubProcessor::getSessionStartInBeats() const noexcept {
    double mn = std::numeric_limits<double>::infinity();
    for (std::uint32_t i = 0; i < gatherer::protocol::NUM_SLOTS; ++i) {
        if (!grid_.per_slot[i].captured.load(std::memory_order_acquire)) continue;
        // Only include slots that contribute to the *current* session — a
        // recording file on disk, or a live recording in progress. This
        // filters out stale per-slot grid info that may linger from ghost
        // slots, deleted recordings, or pre-session-load state — any of
        // which would otherwise pull the session reference to an irrelevant
        // earlier position and shift every other lane's grid.
        const bool has_file = last_recordings_[i] != juce::File{};
        const bool is_live  = recording_active_[i].load(std::memory_order_acquire);
        if (!has_file && !is_live) continue;
        const auto b = grid_.per_slot[i].start_in_beats.load(std::memory_order_relaxed);
        if (b < mn) mn = b;
    }
    return std::isfinite(mn) ? mn : 0.0;
}

void HubProcessor::recomputeSessionLayout() noexcept {
    const auto gi = getCurrentGridInfo();
    const double sr = getSampleRate();
    if (!gi.captured || gi.bpm <= 0.0 || sr <= 0.0) {
        for (int i = 0; i < static_cast<int>(gatherer::protocol::NUM_SLOTS); ++i) {
            playback_.setSlotOffsetSamples(i, 0);
        }
        return;
    }
    const double session_start = getSessionStartInBeats();
    for (int i = 0; i < static_cast<int>(gatherer::protocol::NUM_SLOTS); ++i) {
        const auto sg = getSlotGridInfo(i);
        std::int64_t off = 0;
        // Only honour the slot's grid when it has a real recording or is live —
        // matches the filter in getSessionStartInBeats so the two are consistent.
        const bool has_file = last_recordings_[i] != juce::File{};
        const bool is_live  = recording_active_[i].load(std::memory_order_acquire);
        if (sg.captured && (has_file || is_live)) {
            const double offset_sec = (sg.start_in_beats - session_start) * 60.0 / gi.bpm;
            off = static_cast<std::int64_t>(std::round(offset_sec * sr));
            if (off < 0) off = 0;
        }
        playback_.setSlotOffsetSamples(i, off);
    }
}

void HubProcessor::refreshPlaybackSources() {
    playback_.clearAll();
    for (std::size_t i = 0; i < last_recordings_.size(); ++i) {
        const auto& f = last_recordings_[i];
        if (f != juce::File{} && f.existsAsFile()) {
            playback_.setSourceForSlot(static_cast<int>(i), f);
        }
    }
}

juce::File HubProcessor::getLastRecordingForSlot(int slot) const noexcept {
    if (slot < 0 || slot >= static_cast<int>(last_recordings_.size())) return {};
    return last_recordings_[slot];
}
void HubProcessor::setLastRecordingForSlot(int slot, juce::File f) noexcept {
    if (slot < 0 || slot >= static_cast<int>(last_recordings_.size())) return;
    last_recordings_[slot] = f;
}

bool HubProcessor::isNormalizing() const noexcept {
    return normalizer_ && normalizer_->inProgress();
}

std::vector<OfflineNormalizer::Result> HubProcessor::lastNormalizationResults() const {
    if (!normalizer_) return {};
    return normalizer_->results();
}

void HubProcessor::startCalibration() {
    if (region_ == nullptr || !is_hub_)          return;
    if (calibration_in_progress_)                return;

    // Unique non-zero session id.
    calibration_session_ = static_cast<std::uint64_t>(
        std::chrono::steady_clock::now().time_since_epoch().count());
    if (calibration_session_ == 0) calibration_session_ = 1;

    // Publish session id BEFORE flipping the active flag so any sat that observes
    // `calibration_active = 1` is guaranteed to also see the new session id.
    region_->header.calibration_session_id.store(calibration_session_, std::memory_order_release);
    region_->header.calibration_active.store(1, std::memory_order_release);

    calibration_started_at_   = std::chrono::steady_clock::now();
    calibration_in_progress_  = true;
    last_calibration_result_  = {};
    last_calibration_result_.summary = "Calibrating...";
}

void HubProcessor::finishCalibrationIfReady() {
    if (!calibration_in_progress_) return;

    using namespace std::chrono;
    const auto elapsed = duration_cast<milliseconds>(steady_clock::now() - calibration_started_at_).count();
    if (elapsed < kCalibrationWindowMs) return;

    // End the session.
    region_->header.calibration_active.store(0, std::memory_order_release);

    // Gather responses.
    struct Resp {
        int           slot;
        std::uint64_t hub_hb_at_ack;
        std::uint64_t wp_at_ack;
    };
    std::vector<Resp> resp;
    for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
        const auto& slot = region_->slots[i];
        if (slot.state.load(std::memory_order_acquire) != SLOT_STATE_ACTIVE) continue;
        const auto acked = slot.cal_session_acked.load(std::memory_order_acquire);
        if (acked != calibration_session_) continue;
        Resp r;
        r.slot           = static_cast<int>(i);
        r.hub_hb_at_ack  = slot.cal_start_hub_heartbeat.load(std::memory_order_relaxed);
        r.wp_at_ack      = slot.cal_start_wp.load(std::memory_order_relaxed);
        resp.push_back(r);
    }

    CalibrationResult R;
    R.valid = true;

    if (resp.empty()) {
        R.passed  = false;
        R.summary = "No satellites responded";
        R.detail  = "Either no sats are active, transport is stopped, or the host is "
                    "not delivering processBlock callbacks to any satellite during the "
                    "calibration window. Press play and try again.";
    } else if (resp.size() == 1) {
        R.passed  = true;
        R.summary = "Single satellite — no inter-sat comparison possible";
        char buf[160];
        std::snprintf(buf, sizeof(buf),
                      "Slot %d responded at hub_hb=%llu. Add a second satellite to "
                      "verify inter-sat alignment.",
                      resp[0].slot,
                      static_cast<unsigned long long>(resp[0].hub_hb_at_ack));
        R.detail = buf;
    } else {
        std::uint64_t min_hb = resp.front().hub_hb_at_ack;
        std::uint64_t max_hb = resp.front().hub_hb_at_ack;
        int           min_slot = resp.front().slot;
        int           max_slot = resp.front().slot;
        for (const auto& r : resp) {
            if (r.hub_hb_at_ack < min_hb) { min_hb = r.hub_hb_at_ack; min_slot = r.slot; }
            if (r.hub_hb_at_ack > max_hb) { max_hb = r.hub_hb_at_ack; max_slot = r.slot; }
        }
        const auto offset_callbacks = static_cast<int>(max_hb - min_hb);
        const auto offset_samples   = static_cast<std::int64_t>(offset_callbacks)
                                    * static_cast<std::int64_t>(maxBlockSize());
        R.inter_sat_offset_callbacks = offset_callbacks;
        R.inter_sat_offset_samples   = offset_samples;

        if (offset_callbacks == 0) {
            R.passed  = true;
            R.summary = "Callback-level alignment confirmed";
            char buf[160];
            std::snprintf(buf, sizeof(buf),
                          "All %zu satellites detected the calibration session in the "
                          "same hub callback (hub_hb=%llu).",
                          resp.size(),
                          static_cast<unsigned long long>(min_hb));
            R.detail = buf;
        } else {
            R.passed  = false;
            R.summary = "Callback-level misalignment detected";
            char buf[320];
            std::snprintf(buf, sizeof(buf),
                          "Slot %d detected calibration at hub_hb=%llu, slot %d at hub_hb=%llu. "
                          "Inter-sat offset: %d hub callback%s (~%lld samples). "
                          "Move the hub onto a parent group/bus track so all satellites "
                          "are upstream of it.",
                          max_slot, static_cast<unsigned long long>(max_hb),
                          min_slot, static_cast<unsigned long long>(min_hb),
                          offset_callbacks, offset_callbacks == 1 ? "" : "s",
                          static_cast<long long>(offset_samples));
            R.detail = buf;
        }

        // Audio-content cross-correlation. Catches sample-level offsets that the
        // callback-level check misses (e.g., parallel-topology sub-block jitter).
        //
        // We deliberately read from (wp_now - N - safety) rather than from
        // cal_start_wp. The cal_start_wp data may have been overwritten by sat's
        // overrun policy if the calibration window plus polling latency exceeds the
        // ring capacity (~170ms at 48k). Reading from the recent tail is always safe
        // and still gives a same-Reaper-time slice across sats because both wp's
        // advance in lockstep when sats are aligned.
        constexpr int N = 4096;        // ~85ms at 48k — well within ring capacity
        constexpr int K = 1024;        // search range; covers ±2 blocks at 512 frames
        constexpr float kSignalThreshold = 0.5f;  // normalized corr above this = trust
        constexpr double kSilenceRms     = 1e-5;  // RMS below this = effectively silent

        const auto safety_margin = static_cast<std::uint64_t>(maxBlockSize()) + 64;

        std::vector<std::vector<float>> mono(resp.size());
        std::vector<float> interleaved(static_cast<std::size_t>(N) * RING_CHANNELS);
        bool all_captured = true;
        for (std::size_t i = 0; i < resp.size(); ++i) {
            auto& slot = region_->slots[resp[i].slot];
            SpscRingBuffer rb(slot.ring_header, slot.ring_data, RING_FRAMES, RING_CHANNELS);
            const auto wp_now = rb.writePos();
            if (wp_now < static_cast<std::uint64_t>(N) + safety_margin) {
                all_captured = false;  // not enough audio yet
                break;
            }
            const auto pos = wp_now - static_cast<std::uint64_t>(N) - safety_margin;
            if (!rb.peekAt(pos, interleaved.data(), N)) {
                all_captured = false;
                break;
            }
            mono[i].resize(N);
            for (int j = 0; j < N; ++j) {
                mono[i][j] = 0.5f * (interleaved[j * 2 + 0] + interleaved[j * 2 + 1]);
            }
        }

        if (!all_captured) {
            R.detail = R.detail + " — Audio probe: could not capture ring data "
                       "(ring may have been overrun or not yet have enough samples).";
        }

        if (all_captured) {
            // Precompute energy (and RMS for display) for each captured signal.
            std::vector<double> norm(resp.size(), 0.0);
            std::vector<double> rms (resp.size(), 0.0);
            for (std::size_t i = 0; i < resp.size(); ++i) {
                double s = 0.0;
                for (float x : mono[i]) s += static_cast<double>(x) * x;
                norm[i] = std::sqrt(s);
                rms[i]  = std::sqrt(s / static_cast<double>(N));
            }

            std::string audio_line;
            for (std::size_t i = 0; i < resp.size(); ++i) {
                char buf[64];
                std::snprintf(buf, sizeof(buf), " slot%d:rms=%.4f",
                              resp[i].slot, rms[i]);
                audio_line += buf;
            }

            const auto& a      = mono[0];
            const double norm_a = norm[0];

            int    worst_offset     = 0;
            double worst_corr       = 0.0;
            bool   any_silent       = (rms[0] < kSilenceRms);
            bool   any_uncorrelated = false;

            for (std::size_t i = 1; i < resp.size(); ++i) {
                const auto& b      = mono[i];
                const double norm_b = norm[i];

                if (rms[0] < kSilenceRms || rms[i] < kSilenceRms) {
                    any_silent = true;
                    continue;
                }

                double max_abs_corr = 0.0;
                int    best_k       = 0;
                double best_signed  = 0.0;
                for (int k = -K; k <= K; ++k) {
                    double sum = 0.0;
                    const int j0 = std::max(0, -k);
                    const int j1 = std::min(N, N - k);
                    for (int j = j0; j < j1; ++j) {
                        sum += static_cast<double>(a[j]) * static_cast<double>(b[j + k]);
                    }
                    const double absSum = std::abs(sum);
                    if (absSum > max_abs_corr) {
                        max_abs_corr = absSum;
                        best_k       = k;
                        best_signed  = sum;
                    }
                }

                const double normalized = max_abs_corr / (norm_a * norm_b);
                const char   pol = (best_signed < 0) ? '-' : '+';
                char buf[96];
                std::snprintf(buf, sizeof(buf),
                              " %d↔%d:%+d@%c%.2f",
                              resp[0].slot, resp[i].slot, best_k, pol, normalized);
                audio_line += buf;

                if (normalized < kSignalThreshold) {
                    any_uncorrelated = true;
                } else if (std::abs(best_k) > std::abs(worst_offset)) {
                    worst_offset = best_k;
                    worst_corr   = normalized;
                }
            }

            // Compose the audio-correlation verdict.
            std::string audio_verdict;
            if (any_silent) {
                audio_verdict = "Audio probe: at least one slot was silent — "
                                "cross-correlation skipped for it. "
                                "Play audio in the host while running calibration.";
            } else if (any_uncorrelated) {
                audio_verdict = "Audio probe: low correlation — sats are receiving "
                                "different content. Sample-level offset not "
                                "measurable. (For sample-accurate verification, "
                                "feed the same audio to both sat tracks, e.g. via "
                                "the polarity-null test setup.)";
            } else if (worst_offset == 0) {
                audio_verdict = "Audio probe: ring content is aligned in aggregate "
                                "(0-sample offset across an 85ms window).";
                // If callback-level disagreed (saw a hub_hb difference), flag the
                // parallel-topology race: audio averages out fine over a long
                // window but per-block reads still race, causing block-rate
                // artifacts in the null test.
                if (R.inter_sat_offset_callbacks != 0) {
                    audio_verdict += " HOWEVER: the callback-level probe found a "
                                     "within-callback ordering offset. The two "
                                     "checks disagree because the audio probe "
                                     "averages over a window, while hub's per-block "
                                     "reads happen one block at a time and race "
                                     "against satellite writes each callback. Audible "
                                     "symptom: block-rate artifacts in null test even "
                                     "though gross alignment looks fine. FIX: move the "
                                     "hub to a parent group/bus track so all "
                                     "satellites are processed BEFORE the hub each "
                                     "callback (eliminates the race).";
                }
            } else {
                R.passed                   = false;
                R.inter_sat_offset_samples = worst_offset;
                R.summary                  = "Audio-level sub-block offset detected";
                char buf[256];
                std::snprintf(buf, sizeof(buf),
                              "Inter-sat audio offset: %d samples (correlation %.2f). "
                              "Parallel-topology sub-block jitter — move the hub to "
                              "a parent group/bus track for sample-accurate alignment.",
                              worst_offset, worst_corr);
                audio_verdict = buf;
            }

            R.detail = R.detail + " — " + audio_verdict +
                       (audio_line.empty() ? "" : "  [pairs:" + audio_line + " ]");
        }
    }

    last_calibration_result_ = R;
    calibration_in_progress_ = false;
}

juce::AudioProcessor* JUCE_CALLTYPE createPluginFilter() {
    return new HubProcessor();
}

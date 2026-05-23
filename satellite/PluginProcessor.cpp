#include "PluginProcessor.h"
#include "PluginEditor.h"

#include "protocol/Registry.h"

#include <chrono>
#include <cstring>
#include <thread>

#if defined(_WIN32)
    #include <windows.h>
    static std::uint64_t currentPid() { return static_cast<std::uint64_t>(::GetCurrentProcessId()); }
#else
    #include <unistd.h>
    static std::uint64_t currentPid() { return static_cast<std::uint64_t>(::getpid()); }
#endif

using namespace gatherer;
using namespace gatherer::protocol;

SatelliteProcessor::SatelliteProcessor()
    : juce::AudioProcessor(BusesProperties()
        .withInput("Input",   juce::AudioChannelSet::stereo(), true)
        .withOutput("Output", juce::AudioChannelSet::stereo(), true))
{
    my_uuid_ = generateInstanceId();
    display_name_ = "Sat " + juce::String::toHexString(static_cast<juce::int64>(my_uuid_)).substring(0, 6);
    attachToShm();
}

SatelliteProcessor::~SatelliteProcessor() {
    detachFromShm();
}

void SatelliteProcessor::attachToShm() {
    try {
        shm_ = std::make_unique<SharedMemory>(SHM_NAME, sizeof(SharedRegion),
                                              SharedMemory::Mode::OpenOrCreate);
        region_ = static_cast<SharedRegion*>(shm_->data());

        if (shm_->isOwner()) {
            initializeNewRegion(*region_);
        } else {
            // Brief wait for init_done; the host may load us before the hub.
            const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
            while (!isInitialized(*region_)) {
                if (std::chrono::steady_clock::now() > deadline) break;
                std::this_thread::sleep_for(std::chrono::milliseconds(10));
            }
        }

        region_->header.instance_refcount.fetch_add(1, std::memory_order_acq_rel);

        const int idx = claimSlot(*region_, my_uuid_, currentPid(),
                                  display_name_.toRawUTF8(),
                                  track_name_.toRawUTF8(),
                                  0xFFFFFFFFu);
        slot_index_.store(idx, std::memory_order_release);
    } catch (const std::exception&) {
        shm_.reset();
        region_ = nullptr;
        slot_index_.store(-1, std::memory_order_release);
    }
}

void SatelliteProcessor::detachFromShm() {
    if (region_) {
        const int idx = slot_index_.load(std::memory_order_acquire);
        releaseSlot(*region_, idx, my_uuid_);
        region_->header.instance_refcount.fetch_sub(1, std::memory_order_acq_rel);
        slot_index_.store(-1, std::memory_order_release);
        region_ = nullptr;
    }
    shm_.reset();
}

void SatelliteProcessor::prepareToPlay(double /*sampleRate*/, int samplesPerBlock) {
    scratch_.assign(static_cast<std::size_t>(samplesPerBlock) * RING_CHANNELS, 0.0f);
}

void SatelliteProcessor::releaseResources() {}

bool SatelliteProcessor::isBusesLayoutSupported(const BusesLayout& layouts) const {
    return layouts.getMainInputChannelSet()  == juce::AudioChannelSet::stereo()
        && layouts.getMainOutputChannelSet() == juce::AudioChannelSet::stereo();
}

void SatelliteProcessor::writeInterleavedToRing(const juce::AudioBuffer<float>& buf, int frames) noexcept {
    if (region_ == nullptr) return;
    int idx = slot_index_.load(std::memory_order_acquire);
    if (idx < 0) return;

    // Defensive: the hub's ghost-reclaim policy may have wiped our slot if the host
    // stopped delivering callbacks for >3s (track muted, transport stopped, etc.).
    // If the slot no longer carries our UUID, re-claim a fresh slot.
    if (region_->slots[idx].sat_uuid.load(std::memory_order_acquire) != my_uuid_) {
        idx = claimSlot(*region_, my_uuid_, currentPid(),
                         display_name_.toRawUTF8(),
                         track_name_.toRawUTF8(),
                         0xFFFFFFFFu);
        slot_index_.store(idx, std::memory_order_release);
        if (idx < 0) return;
    }

    auto& slot = region_->slots[idx];

    // Calibration probe response. If the hub has bumped calibration_session_id since
    // we last responded, snapshot (hub_heartbeat, our wp) RIGHT NOW. Hub will compare
    // these snapshots across sats to detect callback-level misalignment with
    // sample-accurate timing.
    if (region_->header.calibration_active.load(std::memory_order_acquire) != 0) {
        const auto current_session = region_->header.calibration_session_id.load(std::memory_order_acquire);
        if (slot.cal_session_acked.load(std::memory_order_relaxed) != current_session) {
            const auto hub_hb = region_->header.hub_heartbeat.load(std::memory_order_acquire);
            SpscRingBuffer rb_for_wp(slot.ring_header, slot.ring_data, RING_FRAMES, RING_CHANNELS);
            const auto my_wp = rb_for_wp.writePos();
            slot.cal_start_hub_heartbeat.store(hub_hb, std::memory_order_relaxed);
            slot.cal_start_wp.store(my_wp, std::memory_order_relaxed);
            // Publish the ack last with release so a hub acquire on cal_session_acked
            // is guaranteed to see the snapshot values too.
            slot.cal_session_acked.store(current_session, std::memory_order_release);
        }
    }

    const auto* L = buf.getReadPointer(0);
    const auto* R = buf.getNumChannels() > 1 ? buf.getReadPointer(1) : L;

    if (scratch_.size() < static_cast<std::size_t>(frames) * RING_CHANNELS) return;
    for (int i = 0; i < frames; ++i) {
        scratch_[static_cast<std::size_t>(i) * RING_CHANNELS + 0] = L[i];
        scratch_[static_cast<std::size_t>(i) * RING_CHANNELS + 1] = R[i];
    }

    // Determine the host frame this block corresponds to BEFORE we publish the ring write.
    // This lets us update anchor_host_frame so that any acquire of write_pos (after the
    // ring write below) sees a consistent (wp, anchor) snapshot.
    std::int64_t hfs_now = 0;
    bool         have_playhead = false;
    if (auto* ph = getPlayHead()) {
        if (auto pos = ph->getPosition()) {
            if (auto t = pos->getTimeInSamples()) {
                hfs_now = *t;
                have_playhead = true;
            }
        }
    }

    SpscRingBuffer rb(slot.ring_header, slot.ring_data, RING_FRAMES, RING_CHANNELS);

    if (have_playhead) {
        // Set the anchor ONLY on the first write into this ring. Subsequent blocks
        // never touch it. Rationale: Reaper's anticipative FX processing can call us
        // with non-monotonic HFS values (it pre-renders blocks and may re-call us for
        // the same range if state changes). Treating those as "discontinuities" caused
        // the anchor to update constantly, which raced against hub reads → crackling.
        //
        // After a DAW seek the anchor will be stale and audio will be misaligned for
        // the rest of the session, but the hub never sees an inconsistent (wp, anchor)
        // pair, which is what matters for clean playback.
        const auto wp_before = rb.writePos();
        if (wp_before == 0) {
            slot.anchor_host_frame.store(hfs_now, std::memory_order_release);
        }
    }

    // Publish the block. The release store on write_pos (inside rb.write) synchronizes
    // with any acquire by the hub — and because anchor_host_frame was stored (with
    // release) above on the first call, the hub sees a consistent (wp, anchor) pair.
    rb.write(scratch_.data(), static_cast<std::uint32_t>(frames));

    slot.sat_heartbeat.fetch_add(1, std::memory_order_relaxed);

    // Diagnostic-only update; hub no longer reads this for alignment.
    if (have_playhead) {
        slot.last_write_host_frame.store(hfs_now + frames, std::memory_order_relaxed);
    }
}

void SatelliteProcessor::processBlock(juce::AudioBuffer<float>& buffer, juce::MidiBuffer&) {
    juce::ScopedNoDenormals noDenormals;
    const int frames = buffer.getNumSamples();
    writeInterleavedToRing(buffer, frames);
    // Honor hub's PDC-calibration solo request: while cali_mute_output is set
    // this sat clears its passthrough output, removing itself from Bitwig's
    // parent-bus mix so hub can isolate a different sat. Sat's SHM ring is
    // unaffected — hub still receives the captured audio there.
    if (region_ != nullptr) {
        const int idx = slot_index_.load(std::memory_order_acquire);
        if (idx >= 0 && idx < static_cast<int>(gatherer::protocol::NUM_SLOTS)) {
            if (region_->slots[idx].cali_mute_output.load(std::memory_order_acquire) != 0u) {
                buffer.clear();
            }
        }
    }
    // Pass-through: leave buffer untouched.
}

juce::AudioProcessorEditor* SatelliteProcessor::createEditor() {
    return new SatelliteEditor(*this);
}

void SatelliteProcessor::getStateInformation(juce::MemoryBlock& destData) {
    juce::ValueTree tree("GathererSat");
    tree.setProperty("uuid_hi", static_cast<juce::int64>(my_uuid_ >> 32), nullptr);
    tree.setProperty("uuid_lo", static_cast<juce::int64>(my_uuid_ & 0xFFFFFFFFull), nullptr);
    tree.setProperty("display_name", display_name_, nullptr);
    juce::MemoryOutputStream stream(destData, false);
    tree.writeToStream(stream);
}

void SatelliteProcessor::setStateInformation(const void* data, int sizeInBytes) {
    auto tree = juce::ValueTree::readFromData(data, static_cast<std::size_t>(sizeInBytes));
    if (! tree.isValid()) return;

    const auto hi = static_cast<std::uint64_t>(static_cast<juce::int64>(tree.getProperty("uuid_hi", 0)));
    const auto lo = static_cast<std::uint64_t>(static_cast<juce::int64>(tree.getProperty("uuid_lo", 0)));
    const auto restored = (hi << 32) | (lo & 0xFFFFFFFFull);

    // Two paths feed this call:
    //   1. Project reload: the original sat is gone, the restored UUID is "free".
    //      We adopt it so hub-side pairings keyed to that UUID survive the reload.
    //   2. Track duplication (e.g. Bitwig): the host copies our state into a brand
    //      new instance while we're still alive. If we adopted the restored UUID
    //      we'd collide with the original. Detect this by scanning for a live
    //      ACTIVE slot already holding it; if found, keep the fresh UUID the
    //      constructor generated.
    bool restored_uuid_is_live = false;
    if (restored != 0 && restored != my_uuid_ && region_ != nullptr) {
        for (std::uint32_t i = 0; i < NUM_SLOTS; ++i) {
            const auto& slot = region_->slots[i];
            if (slot.sat_uuid.load(std::memory_order_acquire) == restored
                && slot.state .load(std::memory_order_acquire) == SLOT_STATE_ACTIVE) {
                restored_uuid_is_live = true;
                break;
            }
        }
    }

    if (restored != 0 && restored != my_uuid_ && !restored_uuid_is_live) {
        if (region_) {
            releaseSlot(*region_, slot_index_.load(std::memory_order_acquire), my_uuid_);
        }
        my_uuid_ = restored;
        if (region_) {
            const int idx = claimSlot(*region_, my_uuid_, currentPid(),
                                      display_name_.toRawUTF8(),
                                      track_name_.toRawUTF8(),
                                      0xFFFFFFFFu);
            slot_index_.store(idx, std::memory_order_release);
        }
        // Only restore the display name on the project-reload path. Duplicates
        // keep the constructor's UUID-derived name so the hub can tell them apart.
        display_name_ = tree.getProperty("display_name", display_name_).toString();
    }
}

void SatelliteProcessor::updateTrackProperties(const TrackProperties& props) {
    track_name_ = props.name.value_or(juce::String{});
    if (region_ != nullptr) {
        const int idx = slot_index_.load(std::memory_order_acquire);
        if (idx >= 0) {
            auto& slot = region_->slots[idx];
            std::memset(slot.track_name, 0, sizeof(slot.track_name));
            std::strncpy(slot.track_name, track_name_.toRawUTF8(), sizeof(slot.track_name) - 1);
        }
    }
}

bool SatelliteProcessor::isHubConnected() const noexcept {
    return region_ != nullptr
        && region_->header.hub_uuid.load(std::memory_order_acquire) != 0;
}

juce::String SatelliteProcessor::getDisplayName() const { return display_name_; }
juce::String SatelliteProcessor::getTrackName()   const { return track_name_; }

// JUCE plugin entry point.
juce::AudioProcessor* JUCE_CALLTYPE createPluginFilter() {
    return new SatelliteProcessor();
}

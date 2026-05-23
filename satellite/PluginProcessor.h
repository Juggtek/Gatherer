#pragma once

#include <JuceHeader.h>

#include <atomic>
#include <cstdint>
#include <memory>

#include "protocol/SharedRegion.h"
#include "ringbuffer/SpscRingBuffer.h"
#include "shm/SharedMemory.h"

class SatelliteProcessor : public juce::AudioProcessor {
public:
    SatelliteProcessor();
    ~SatelliteProcessor() override;

    void prepareToPlay(double sampleRate, int samplesPerBlock) override;
    void releaseResources() override;
    bool isBusesLayoutSupported(const BusesLayout& layouts) const override;
    void processBlock(juce::AudioBuffer<float>&, juce::MidiBuffer&) override;

    juce::AudioProcessorEditor* createEditor() override;
    bool hasEditor() const override { return true; }

    const juce::String getName() const override { return JucePlugin_Name; }
    bool acceptsMidi() const override { return false; }
    bool producesMidi() const override { return false; }
    bool isMidiEffect() const override { return false; }
    double getTailLengthSeconds() const override { return 0.0; }

    int getNumPrograms() override { return 1; }
    int getCurrentProgram() override { return 0; }
    void setCurrentProgram(int) override {}
    const juce::String getProgramName(int) override { return {}; }
    void changeProgramName(int, const juce::String&) override {}

    void getStateInformation(juce::MemoryBlock& destData) override;
    void setStateInformation(const void* data, int sizeInBytes) override;

    void updateTrackProperties(const TrackProperties& props) override;

    // Editor helpers.
    int           getSlotIndex()   const noexcept { return slot_index_.load(std::memory_order_acquire); }
    std::uint64_t getUuid()        const noexcept { return my_uuid_; }
    bool          isHubConnected() const noexcept;
    juce::String  getDisplayName() const;
    juce::String  getTrackName()   const;

private:
    void attachToShm();
    void detachFromShm();
    void writeInterleavedToRing(const juce::AudioBuffer<float>& buf, int frames) noexcept;

    std::unique_ptr<gatherer::SharedMemory> shm_;
    gatherer::protocol::SharedRegion*       region_ = nullptr;

    std::uint64_t        my_uuid_   = 0;
    std::atomic<int>     slot_index_{-1};

    juce::String         display_name_; // persisted in plugin state
    juce::String         track_name_;   // from updateTrackProperties

    std::vector<float>   scratch_;       // interleaved write scratch, sized in prepareToPlay

    // PDC calibration: when hub sets the SHM `inject_spike` flag, sat sees
    // it on its next processBlock. To avoid all live sats injecting their
    // spike in the SAME block (which would superimpose them in hub's
    // input and make per-slot identification impossible), each sat waits
    // `slot_index` additional blocks before firing. So slot 0 injects
    // immediately, slot 1 one block later, etc. Hub then knows exactly
    // which block to look in for each sat's spike.
    int                  cali_block_countdown_ = -1;  // -1 = idle
};

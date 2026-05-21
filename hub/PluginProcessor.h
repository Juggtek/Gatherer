#pragma once

#include <JuceHeader.h>

#include <array>
#include <atomic>
#include <chrono>
#include <cstdint>
#include <memory>
#include <string>

#include "measurement/Loudness.h"
#include "protocol/SharedRegion.h"
#include "ringbuffer/SpscRingBuffer.h"
#include "shm/SharedMemory.h"
#include "recording/Recorder.h"
#include "recording/Normalizer.h"
#include "playback/PlaybackEngine.h"
#include "session/SessionManager.h"
#include "undo/CommandStack.h"

class HubProcessor : public juce::AudioProcessor {
public:
    HubProcessor();
    ~HubProcessor() override;

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

    // Editor helpers.
    bool          isHub()           const noexcept { return is_hub_; }
    int           activeSatellites() const noexcept;
    std::uint64_t hubHeartbeat()    const noexcept;
    std::uint32_t maxBlockSize()    const noexcept;

    // Per-slot levels for the UI meters. Per-channel: peak / rms in dB.
    // Updated in the audio thread each block; safe to read at GUI rate.
    struct LevelSnapshot {
        float peak_db_l;
        float rms_db_l;
        float peak_db_r;
        float rms_db_r;
    };
    LevelSnapshot getSlotLevels(int slot_index) const noexcept;

    // Per-slot AudioThumbnail, fed live by the recording writer. Lives across
    // the plugin's lifetime; reset() each time recording starts. The editor's
    // LayerLane component pulls this and paints.
    juce::AudioThumbnail* getThumbnail(int slot_index) const noexcept;

    // EBU R128 loudness snapshot per slot. Integrated, momentary, short-term
    // in LUFS. Values below ~-100 indicate "no measurable signal".
    struct LufsSnapshot { float integrated; float momentary; float short_term; };
    LufsSnapshot getSlotLufs(int slot_index) const noexcept;

    // Session-wide target loudness for normalization. Default -14 LUFS
    // (Spotify / streaming reference). Persisted as part of the plugin state.
    float getTargetLufs()              const noexcept { return target_lufs_.load(std::memory_order_relaxed); }
    void  setTargetLufs(float v)             noexcept { target_lufs_.store(v, std::memory_order_relaxed); }

    // Sets the slot's gain so its current integrated LUFS lands on the target.
    // No-op if the slot has no measurable integrated value yet. Returns true on
    // success, false if no usable LUFS reading was available.
    bool normalizeSlotGainToTarget(int slot_index) noexcept;

    // Per-slot mixer state. Reads/writes from any thread (atomic). Persisted as
    // part of the plugin state.
    bool  getMute(int slot)              const noexcept;
    void  setMute(int slot, bool on)           noexcept;
    bool  getSolo(int slot)              const noexcept;
    void  setSolo(int slot, bool on)           noexcept;
    float getGainDb(int slot)            const noexcept;
    void  setGainDb(int slot, float db)        noexcept;

    bool  getRecordArm(int slot)         const noexcept;
    void  setRecordArm(int slot, bool on)      noexcept;

    // Per-slot normalize gain stage — independent from the user-facing gain fader.
    // Updated by the "N" button on a row (or normalizeAllActiveSlots) and reset
    // explicitly via resetNormalize. Default 0 dB (linear 1.0).
    float getNormalizeDb(int slot)              const noexcept;
    void  setNormalizeDb(int slot, float db)          noexcept;
    void  resetNormalize(int slot)                    noexcept { setNormalizeDb(slot, 0.0f); }

    // Per-slot target LUFS — defaults to the global target value when a row is
    // created. Persisted alongside the rest of the mix state.
    float getSlotTargetLufs(int slot)           const noexcept;
    void  setSlotTargetLufs(int slot, float v)        noexcept;

    // Loop over every active slot and normalize each one to its per-slot target.
    void  normalizeAllActiveSlots()                   noexcept;

    // Undo / redo stack. UI actions (button clicks, slider changes, edits) push
    // commands here; the raw setX() methods above stay direct-write for paths
    // that shouldn't be undoable (state restore, the audio thread, the
    // ghost-reclaim cleanup, etc.).
    gatherer::undo::CommandStack&       commandStack()       noexcept { return command_stack_; }
    const gatherer::undo::CommandStack& commandStack() const noexcept { return command_stack_; }

    // Playback engine — file-based multi-track playback through the hub's mix
    // path. The editor drives transport via this accessor; processBlock pulls
    // from it instead of the sat ring when transport is Playing and the slot
    // has a loaded source.
    gatherer::playback::Engine&       playback()       noexcept { return playback_; }
    const gatherer::playback::Engine& playback() const noexcept { return playback_; }

    // Session manager — owns the current session folder and manifest.json I/O.
    gatherer::session::SessionManager&       session()       noexcept { return session_; }
    const gatherer::session::SessionManager& session() const noexcept { return session_; }

    // Reload all playback sources from the per-slot last recordings. Called
    // automatically when recording stops; the editor can also trigger it
    // manually (e.g. after deleting/restoring a recording).
    void refreshPlaybackSources();

    // Beat-grid info captured from the host while recording.
    //
    // Session-shared (constant for the session): bpm + time signature. Captured
    // the first time we observe an active recording with the DAW playing.
    //
    // Per-slot (varies per recording): start_in_seconds / start_in_beats — the
    // DAW PPQ at the recording's sample 0. Each slot captures its own at the
    // first audio block of its own recording, so re-recording one track from a
    // different DAW position aligns correctly on that lane independently.
    struct GridInfo {
        bool   captured         = false;        // session-level captured
        double bpm              = 120.0;
        int    time_sig_num     = 4;
        int    time_sig_den     = 4;
        // Reference start (the first slot to capture in this session). Kept
        // mainly for compatibility with old manifests; per-slot is the source
        // of truth for drawing.
        double start_in_seconds = 0.0;
        double start_in_beats   = 0.0;
    };
    GridInfo getCurrentGridInfo() const noexcept;
    void     setCurrentGridInfo(const GridInfo& g) noexcept;
    void     resetGrid() noexcept;

    struct SlotGridInfo {
        bool   captured         = false;
        double start_in_seconds = 0.0;
        double start_in_beats   = 0.0;
    };
    SlotGridInfo getSlotGridInfo(int slot) const noexcept;
    void         setSlotGridInfo(int slot, const SlotGridInfo& g) noexcept;

    // Earliest start_in_beats across all captured slots — defines the session's
    // "x=0" in the lane time axis. Returns 0 if no slot has captured grid yet.
    double getSessionStartInBeats() const noexcept;

    // Recompute each playback source's offset within the session timeline based
    // on its slot grid info. Called after stopRecording and session restore so
    // slots that started at different DAW positions render and play back with
    // the correct relative offset.
    void   recomputeSessionLayout() noexcept;

    // Recording lifecycle. startRecording() collects all currently-armed active
    // slots and launches background writers; stopRecording() flushes and
    // finalizes the WAVs. Idempotent.
    bool        startRecording();
    void        stopRecording();
    bool        isRecording() const noexcept;
    juce::File  currentRecordingFolder() const;

    // Armed-pending: user clicked Record while DAW transport was stopped. The
    // hub waits until the host transport starts before actually creating the
    // writers (so the file's sample 0 = first sample after play, with no
    // leading silence and no grid offset from accumulated paused-state writes).
    bool        isArmedPending()     const noexcept { return armed_pending_.load(std::memory_order_acquire); }
    bool        isArmedOrRecording() const noexcept { return isArmedPending() || isRecording(); }

    // Explicit "Export Normalized" — renders `*_normalized.wav` siblings for
    // every loaded recording using the **current per-slot norm_db** (so the
    // exported file matches the live preview). Returns false if there's
    // nothing to render. Status is queried via isNormalizing() while it runs.
    bool exportNormalized();
    bool                                       isNormalizing() const noexcept;
    std::vector<OfflineNormalizer::Result>     lastNormalizationResults() const;

    // Each slot's most recent recording (set by stopRecording, cleared by
    // delete). Used by the per-row "Delete recording" action.
    juce::File getLastRecordingForSlot(int slot) const noexcept;
    void       setLastRecordingForSlot(int slot, juce::File f) noexcept;

    // "Include track input as a source" toggle.
    //
    // OFF (default for VST3 in-DAW): processBlock clears the input buffer and outputs
    //   only the sat-ring mix. The hub's own track input is discarded — appropriate
    //   when the hub sits on a parent group bus and you don't want sat passthrough
    //   double-counted, or when the hub is on an empty track used as a "sink".
    //
    // ON (default for the Standalone deployment): the input buffer (track audio in a
    //   DAW, system audio device input in standalone) is kept and the sat-ring mix is
    //   summed on top. Useful for monitoring live input mixed with gathered sats.
    bool isIncludeTrackInput()  const noexcept { return include_track_input_.load(std::memory_order_relaxed); }
    void setIncludeTrackInput(bool v) noexcept { include_track_input_.store(v, std::memory_order_relaxed); }

    // Silence-padding toggle for recordings. When ON, every armed slot's WAV
    // begins at session play-start, with leading zeros for any sat that
    // didn't write yet (some hosts gate processBlock by clip presence — e.g.
    // Bitwig). When OFF, each WAV begins at its sat's actual first-write
    // moment so the file is tight (no silence overhead). The toggle is read
    // at record-start time and applied to that whole take.
    bool isPadSilenceInRecord()  const noexcept { return pad_silence_in_record_.load(std::memory_order_relaxed); }
    void setPadSilenceInRecord(bool v) noexcept { pad_silence_in_record_.store(v, std::memory_order_relaxed); }

    // Audio-thread per-slot counter: incremented by `frames` each processBlock
    // while a slot is recording_active and transport is playing. Writers read
    // this to know how much silence (if any) to pad when their sat ring isn't
    // producing samples but the DAW transport is rolling.
    std::atomic<std::uint64_t>* expectedSamplesPtr(int slot) noexcept {
        if (slot < 0 || slot >= static_cast<int>(expected_samples_.size())) return nullptr;
        return &expected_samples_[slot];
    }

    // True when this AudioProcessor is being hosted by JUCE's standalone wrapper
    // (`Gatherer Hub.app`) rather than a DAW. Editors use it to choose deployment-
    // specific defaults / UI hints.
    bool isStandaloneDeployment() const noexcept { return wrapperType == wrapperType_Standalone; }

    // Active calibration probe. Triggers a session, then gathers the per-sat
    // (hub_heartbeat, wp) snapshots and produces a sample-accurate report.
    struct CalibrationResult {
        bool         valid = false;
        bool         passed = false;
        std::string  summary;
        std::string  detail;
        int          inter_sat_offset_callbacks = 0;
        std::int64_t inter_sat_offset_samples   = 0;
    };

    // Begin a calibration session. Non-blocking. Caller polls via
    // calibrationInProgress() and reads result via lastCalibrationResult() once
    // it returns false.
    void startCalibration();
    bool calibrationInProgress() const noexcept { return calibration_in_progress_; }
    CalibrationResult lastCalibrationResult() const { return last_calibration_result_; }

    // Internal — called by the editor's timer once enough time has passed since
    // startCalibration() so sats have had a chance to ack the session.
    void finishCalibrationIfReady();

    struct SatelliteSnapshot {
        int           slot_index;
        std::uint64_t uuid;
        juce::String  display_name;
        juce::String  track_name;
        std::uint32_t heartbeat;
        std::uint64_t write_pos;
        std::int64_t  last_write_host_frame;
        std::uint32_t color_rgba;
    };
    std::vector<SatelliteSnapshot> snapshotSatellites() const;

    // Scan ACTIVE slots and forcibly reclaim any whose owning process is no
    // longer alive (PID-liveness check). Called explicitly: once on hub attach
    // to clean up stale slots left over from prior DAW processes, and on the
    // "Re-analyze" button so users have a manual trigger. NOT called on a
    // timer — a running sat that's been idle (DAW transport stopped, track
    // bypassed) is still alive and should keep its slot.
    void reclaimGhostSlots();

private:
    void attachToShm();
    void detachFromShm();

    std::unique_ptr<gatherer::SharedMemory> shm_;
    gatherer::protocol::SharedRegion*       region_ = nullptr;

    std::uint64_t my_uuid_ = 0;
    bool          is_hub_  = false;

    int max_block_size_ = 0;
    std::vector<float> scratch_;  // per-block read scratch

    // Per-slot live levels (linear amplitude), split L/R. Updated by processBlock
    // for any slot that contributes audio to the mix; read by the editor at GUI
    // rate.
    struct LiveLevels {
        std::atomic<float> peak_lin_l { 0.0f };
        std::atomic<float> rms_lin_l  { 0.0f };
        std::atomic<float> peak_lin_r { 0.0f };
        std::atomic<float> rms_lin_r  { 0.0f };
    };
    std::array<LiveLevels, gatherer::protocol::NUM_SLOTS> levels_{};

    // Thumbnail infrastructure — one shared cache, one thumbnail per slot. The
    // format manager is needed by AudioThumbnail's ctor; we don't actually open
    // any files from disk through it, the thumbnails are fed live via addBlock.
    juce::AudioFormatManager                                 thumb_format_manager_;
    juce::AudioThumbnailCache                                thumb_cache_ { gatherer::protocol::NUM_SLOTS };
    std::array<std::unique_ptr<juce::AudioThumbnail>,
               gatherer::protocol::NUM_SLOTS>                thumbnails_;

    // Per-slot mixer state. Gain stored as linear amplitude (used directly by
    // processBlock). UI works in dB and converts.
    //
    // Signal flow per slot: sat_ring → norm_gain → gain (user fader) → mute/solo gate → mix.
    // norm_gain is the LUFS-normalization stage (separate from the fader so the
    // upcoming Adaptive Mixer can drive the fader without fighting normalization).
    struct MixState {
        std::atomic<bool>  mute        { false };
        std::atomic<bool>  solo        { false };
        std::atomic<float> gain_lin    { 1.0f };
        std::atomic<float> norm_lin    { 1.0f };
        std::atomic<float> target_lufs { -14.0f };  // per-slot target; defaults from global
        std::atomic<bool>  record_arm  { false };
    };
    std::array<MixState, gatherer::protocol::NUM_SLOTS> mix_{};

    std::unique_ptr<Recorder>          recorder_;
    std::unique_ptr<OfflineNormalizer> normalizer_;
    gatherer::undo::CommandStack       command_stack_;
    gatherer::playback::Engine         playback_;
    gatherer::session::SessionManager  session_;
    std::array<juce::File, gatherer::protocol::NUM_SLOTS> last_recordings_{};

    // Deployment-aware parameter. Default depends on wrapperType (see constructor).
    std::atomic<bool> include_track_input_{ false };
    std::atomic<bool> pad_silence_in_record_{ true };

    std::atomic<float> target_lufs_{ -14.0f };

    std::array<std::atomic<std::uint64_t>, gatherer::protocol::NUM_SLOTS> expected_samples_{};

    // Calibration probe state. Started by startCalibration(), finalized when the
    // editor's timer notices enough time has passed.
    bool                                              calibration_in_progress_ = false;
    std::uint64_t                                     calibration_session_     = 0;
    std::chrono::steady_clock::time_point             calibration_started_at_;
    CalibrationResult                                 last_calibration_result_;
    // Window between starting calibration and gathering results. Kept short so
    // (a) sats have time to ack but (b) the wait + polling latency don't exceed
    // the ring capacity (~170ms at 48k/8192-frame rings). The audio cross-correlation
    // reads from wp_now anyway, so this is mainly about latency between click and
    // verdict on screen.
    static constexpr int                              kCalibrationWindowMs = 50;

    // Per-slot tracking. Only two values: the wp we saw last time (to suppress duplicate
    // reads when Reaper calls processBlock more often than the satellites are advancing)
    // and the slot's UUID (to detect a reclaim). Note we do NOT cache local_rp or any
    // "lag" — the read position is recomputed each callback from current wp and current
    // max_block_size_, so we're immune to whatever value samplesPerBlock had at init.
    struct SlotState {
        std::uint64_t last_seen_wp = 0;
        std::uint64_t last_uuid    = 0;
    };
    std::array<SlotState, gatherer::protocol::NUM_SLOTS> slot_states_{};

    // Ghost-slot reclaim is PID-based — no per-slot tracking needed. See
    // reclaimGhostSlots() for the policy.

    // Grid storage. The audio thread writes the value fields ONCE per session
    // (the first processBlock observing recording-active && !captured) then
    // publishes via the release store on `captured`. After that, the audio
    // thread no longer writes. The GUI thread reads through getCurrentGridInfo
    // with an acquire load on `captured`. Session-restore writes from the
    // message thread when no recording is active.
    struct GridStorage {
        // Session-shared
        double bpm          = 120.0;
        int    time_sig_num = 4;
        int    time_sig_den = 4;
        std::atomic<bool> captured { false };  // session-level captured flag

        // Per-slot
        struct PerSlot {
            std::atomic<double> start_in_seconds { 0.0 };
            std::atomic<double> start_in_beats   { 0.0 };
            std::atomic<bool>   captured         { false };
        };
        std::array<PerSlot, gatherer::protocol::NUM_SLOTS> per_slot;
    };
    GridStorage grid_;

    // Per-slot recording snapshot — set in startRecording, cleared on stop.
    // Used by the grid-capture path to back-correct the captured PPQ for the
    // (small) latency between message-thread record-press and the first audio
    // block we get a chance to read the playhead on. delta = sat.wp_now − start_wp.
    std::array<std::atomic<bool>,          gatherer::protocol::NUM_SLOTS> recording_active_{};
    std::array<std::atomic<std::uint64_t>, gatherer::protocol::NUM_SLOTS> recording_start_wp_{};

    // Armed-but-not-yet-recording state. startRecording (message thread) only
    // flips `armed_pending_`; the audio thread does the actual sat.wp
    // snapshot the next block where the host transport is playing, then posts
    // a callAsync that turns the snapshot into writers + a Recorder::start.
    // Putting the wp loop on the audio thread is what makes the snapshot
    // race-free: a single processBlock invocation reads every slot's
    // write_pos at one atomic point in time, so all slots agree on a single
    // "this is where the recording begins" DAW frame.
    std::atomic<bool>                  armed_pending_       { false };
    std::atomic<bool>                  last_seen_playing_   { false };
    std::atomic<bool>                  play_trigger_posted_ { false };

    bool actuallyStartRecording();

    // Per-slot EBU R128 loudness state. Analyzer is (re)created in prepareToPlay;
    // atomics are written by the audio thread and read by the GUI thread.
    struct SlotLufs {
        std::unique_ptr<gatherer::measurement::LoudnessAnalyzer> analyzer;
        std::atomic<float> integrated  { -100.0f };
        std::atomic<float> momentary   { -100.0f };
        std::atomic<float> short_term  { -100.0f };
        int                query_counter = 0;
    };
    std::array<SlotLufs, gatherer::protocol::NUM_SLOTS> lufs_{};
};

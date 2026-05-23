#pragma once

#include <JuceHeader.h>

#include <array>
#include <atomic>
#include <chrono>
#include <cstdint>
#include <limits>
#include <memory>
#include <string>
#include <vector>

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
    // every loaded recording using the **current per-slot norm_db**, padded to
    // the session timeline (per-slot offset + trailing silence to the longest
    // recording's end). The exported files form a sample-aligned stem set
    // that can be loaded into another DAW and dropped at the project origin.
    bool exportNormalized();

    // Same idea without the normalize gain — produces `*_aligned.wav` stems
    // whose only difference from the original WAVs is alignment to the
    // session timeline (offsets + trailing silence).
    bool exportAligned();
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

    // --- Per-sat PDC measurement ---------------------------------------------
    // Each sat's audio at its ring's wp = X represents music position
    // (master_at_wp_X + D) — D > 0 means the DAW pre-rolled this sat's
    // upstream chain (typically a latent sampler) so its output reaches
    // master in time. We can't read D from any host API (Bitwig passes a
    // uniform master playhead to every plugin regardless of track PDC), so
    // we measure it by cross-correlating each sat's SHM stream against the
    // hub's own input — which is PDC-aligned by the DAW and so serves as
    // the absolute-time reference.
    //
    // `pdc_d_samples_[i]` is the latest *measured* D for slot i. Updated by
    // the PdcCalibrator background thread. INT64_MIN sentinel = not yet
    // measured. `pdc_d_override_[i]` is a user-set value that takes
    // precedence (UI: editable per-track field) when not INT64_MIN.
    std::int64_t pdcDMeasured(int slot) const noexcept {
        if (slot < 0 || slot >= static_cast<int>(pdc_d_samples_.size())) return kPdcUnknown;
        return pdc_d_samples_[static_cast<std::size_t>(slot)].load(std::memory_order_relaxed);
    }
    std::int64_t pdcDOverride(int slot) const noexcept {
        if (slot < 0 || slot >= static_cast<int>(pdc_d_override_.size())) return kPdcUnknown;
        return pdc_d_override_[static_cast<std::size_t>(slot)].load(std::memory_order_relaxed);
    }
    // Returns the value the writer/recorder should actually apply: override
    // if set, else measured (only if VERY confident), else 0.
    //
    // The threshold is intentionally high (0.7) because cross-correlation
    // against a busy mix is noisy: spurious peaks routinely land between
    // 0.2-0.5. Applying a wrong D makes recordings strictly worse than no
    // compensation, so we only apply when the peak is unambiguous. Users
    // with tracks that the auto-measurement can't reach (sustained pads,
    // quiet content, short clips) can set the override manually from
    // their DAW's reported per-track latency.
    static constexpr float kPdcMinConfidence = 0.70f;
    std::int64_t pdcDEffective(int slot) const noexcept {
        const auto ov = pdcDOverride(slot);
        if (ov != kPdcUnknown) return ov;
        const auto m = pdcDMeasured(slot);
        if (m == kPdcUnknown) return 0;
        if (pdcConfidence(slot) < kPdcMinConfidence) return 0;
        // Reject measurements pegged at the search boundary — those are
        // almost always wrong (either no real peak, or true D is beyond
        // the search range and we landed at the edge).
        const auto abs_m = m < 0 ? -m : m;
        // boundary equals the calibrator's K (= 4096 samples). A measurement
        // landing within 8 samples of the boundary almost certainly means
        // the true peak is outside the search range — reject it.
        constexpr std::int64_t kBoundaryGuard = 4088;
        if (abs_m >= kBoundaryGuard) return 0;
        return m;
    }
    void setPdcDOverride(int slot, std::int64_t samples) noexcept {
        if (slot < 0 || slot >= static_cast<int>(pdc_d_override_.size())) return;
        pdc_d_override_[static_cast<std::size_t>(slot)].store(samples, std::memory_order_relaxed);
    }
    void clearPdcDOverride(int slot) noexcept {
        if (slot < 0 || slot >= static_cast<int>(pdc_d_override_.size())) return;
        pdc_d_override_[static_cast<std::size_t>(slot)].store(kPdcUnknown, std::memory_order_relaxed);
    }
    static constexpr std::int64_t kPdcUnknown = std::numeric_limits<std::int64_t>::min();

    // Per-sat solo-calibration: hub iterates active sats, mutes every sat's
    // output except the one being measured, lets the ref ring fill with the
    // isolated sat's content, then cross-correlates. Triggered by the
    // Calibrate button when transport is playing. State is published so the
    // UI can show a progress indicator.
    void startSoloCalibration();
    void cancelSoloCalibration();
    enum class SoloCaliState : std::uint8_t { Idle, Capturing, Measuring, Done, Failed };
    struct SoloCaliStatus {
        SoloCaliState state             = SoloCaliState::Idle;
        int           current_slot      = -1;
        int           total_slots       = 0;
        int           completed_slots   = 0;
        std::string   message;
    };
    SoloCaliStatus soloCaliStatus() const noexcept;

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

    // Display ordering — independent of SHM slot index. display_order_[i] is
    // the slot_index shown at display position i. Defaults to identity; the
    // user can permute via the up/down buttons on each LayerRow. Persisted in
    // plugin state + session manifest so re-routings survive reloads.
    //
    // The Adaptive Mixer will later key its rules off display position rather
    // than raw slot index so the user's spatial arrangement of layers carries
    // semantic weight (e.g. "topmost layer is bus / submix").
    std::array<int, gatherer::protocol::NUM_SLOTS> getDisplayOrder() const noexcept;
    void setDisplayOrder(const std::array<int, gatherer::protocol::NUM_SLOTS>& order) noexcept;
    // Move a slot one position up (-1) or down (+1) in display order; clamps
    // to the array bounds and silently no-ops at the edges.
    void moveSlotInDisplayOrder(int slot, int direction) noexcept;

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
    bool launchAlignedExport(bool apply_normalize, const juce::String& suffix);

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

    // Display-order permutation. display_order_[i] = slot_index at position i.
    // Only touched by the message thread (UI / manifest I/O).
    std::array<int, gatherer::protocol::NUM_SLOTS> display_order_{};

    // Per-slot snapshot from the previous processBlock — used as the snapshot
    // anchor at the play-rising-edge so recording sample 0 lines up with the
    // first sample of the play-start block (rather than the next block, which
    // is what reading sat.wp_now *after* sat's upstream write would give).
    //
    // Audio-thread-only. UUID is tracked alongside so a sat reclaim between
    // blocks invalidates the stale wp (we fall back to the current value).
    struct PrevSlotSnapshot {
        std::uint64_t wp   = 0;
        std::uint64_t uuid = 0;
    };
    std::array<PrevSlotSnapshot, gatherer::protocol::NUM_SLOTS> prev_slot_state_{};

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
    // results are read by the GUI thread atomically.
    //
    // **Owned exclusively by the lufs worker thread** — not the audio thread.
    // libebur128's per-block processing (and especially `loudness_global`) is
    // O(history) and at small DAW buffer sizes (e.g. Bitwig's 80-sample blocks)
    // routinely blows the audio deadline. We do the feed + query on a
    // dedicated background thread, which reads each sat's ring directly via
    // peekAt and publishes results through atomic stores for the GUI.
    struct SlotLufs {
        std::unique_ptr<gatherer::measurement::LoudnessAnalyzer> analyzer;
        std::atomic<float> integrated  { -100.0f };
        std::atomic<float> momentary   { -100.0f };
        std::atomic<float> short_term  { -100.0f };
        // Worker-thread-only state (no atomics needed — single-thread access).
        std::uint64_t      last_fed_wp    = 0;
        std::uint64_t      last_seen_uuid = 0;  // detects slot turnover
        int                query_counter  = 0;
    };
    std::array<SlotLufs, gatherer::protocol::NUM_SLOTS> lufs_{};

    // Worker thread that feeds the per-slot LoudnessAnalyzers and queries them
    // at a sub-100Hz cadence. Created in prepareToPlay so we know the host's
    // sample rate; torn down on releaseResources and destruction.
    class LufsWorker : public juce::Thread {
    public:
        explicit LufsWorker(HubProcessor& p)
            : juce::Thread("GathererLufs"), processor_(p) {}
        void run() override;
    private:
        HubProcessor& processor_;
    };
    std::unique_ptr<LufsWorker> lufs_worker_;
    void lufsWorkerTick();

    // --- PDC measurement -----------------------------------------------------
    // Hub captures its input audio (PDC-aligned mix from the parent bus) into
    // a mono reference ring. The PdcCalibrator worker periodically cross-
    // correlates each active sat's SHM stream against this ring to estimate
    // per-sat D.
    static constexpr std::uint32_t kPdcRefCapacity = 524288u;  // ~10.9s @ 48k, power of two
                                                                // (big enough that clip-gated sats
                                                                // whose last write was many seconds ago
                                                                // can still be correlated against
                                                                // recent hub data)
    gatherer::SpscRingBuffer::Header   pdc_ref_header_{};
    std::vector<float>                 pdc_ref_data_;  // size = kPdcRefCapacity, mono
    std::atomic<std::int64_t>          pdc_ref_anchor_host_frame_{ 0 };  // master frame at hub's first ref-ring write
    std::atomic<bool>                  pdc_ref_anchor_set_{ false };

    std::array<std::atomic<std::int64_t>, gatherer::protocol::NUM_SLOTS> pdc_d_samples_;
    std::array<std::atomic<std::int64_t>, gatherer::protocol::NUM_SLOTS> pdc_d_override_;
    // Debug counters — increment per calibrator iteration / per successful
    // measurement so the UI can show "is the calibrator even alive?".
    std::atomic<std::uint64_t>                                            pdc_tick_count_       { 0 };
    std::atomic<std::uint64_t>                                            pdc_success_count_    { 0 };

public:
    std::uint64_t pdcTickCount()    const noexcept { return pdc_tick_count_.load(std::memory_order_relaxed); }
    std::uint64_t pdcSuccessCount() const noexcept { return pdc_success_count_.load(std::memory_order_relaxed); }

    // Per-slot last-skip reason. Empty string = either the slot is not
    // active, or the last attempt succeeded.
    enum class PdcSkip : std::uint8_t {
        Ok = 0,
        SlotInactive,
        SatNotWritten,
        SatNotEnoughData,
        SatRingOverrun,
        SatSilent,
        HubWindowBeforeZero,
        HubWindowPastWrite,
        HubWindowOutOfCap,
        HubSilent,
    };
    PdcSkip pdcLastSkip(int slot) const noexcept {
        if (slot < 0 || slot >= static_cast<int>(pdc_last_skip_.size())) return PdcSkip::SlotInactive;
        return static_cast<PdcSkip>(pdc_last_skip_[static_cast<std::size_t>(slot)].load(std::memory_order_relaxed));
    }
    // Normalized correlation coefficient at the peak lag (0..1). Near 1 =
    // strong match. Near 0 = no real match found (random noise peak).
    float pdcConfidence(int slot) const noexcept {
        if (slot < 0 || slot >= static_cast<int>(pdc_confidence_.size())) return 0.0f;
        return pdc_confidence_[static_cast<std::size_t>(slot)].load(std::memory_order_relaxed);
    }
private:
    std::array<std::atomic<std::uint8_t>, gatherer::protocol::NUM_SLOTS> pdc_last_skip_{};
    std::array<std::atomic<float>,        gatherer::protocol::NUM_SLOTS> pdc_confidence_{};

    class PdcCalibrator : public juce::Thread {
    public:
        explicit PdcCalibrator(HubProcessor& p)
            : juce::Thread("GathererPdc"), processor_(p) {}
        void run() override;
    private:
        HubProcessor& processor_;
    };
    std::unique_ptr<PdcCalibrator> pdc_calibrator_;
    void runSpikeCalibration();
    bool measureOneSatSpike(int slot);

    // Calibration state (driven by PdcCalibrator's run loop when
    // `solo_cali_active_` is set). "Solo cali" naming is legacy from the
    // previous mute-others approach; current mechanism is spike injection.
    std::atomic<bool>            solo_cali_active_      { false };
    std::atomic<int>             solo_cali_current_     { -1 };
    std::atomic<int>             solo_cali_total_       { 0 };
    std::atomic<int>             solo_cali_completed_   { 0 };
    std::atomic<std::uint8_t>    solo_cali_state_       { static_cast<std::uint8_t>(SoloCaliState::Idle) };
    juce::CriticalSection        solo_cali_message_lock_;
    std::string                  solo_cali_message_;
};

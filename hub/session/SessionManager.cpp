#include "SessionManager.h"

#include "../PluginProcessor.h"

namespace gatherer::session {

namespace {
constexpr int kManifestVersion = 1;
const char* kManifestName = "manifest.json";

juce::String timestampNow() {
    return juce::Time::getCurrentTime().formatted("%Y-%m-%d_%H-%M-%S");
}

// Refresh all per-slot thumbnails from WAVs present in the session folder. Slots
// without a corresponding file get cleared.
void rebuildThumbnailsFromFolder(HubProcessor& p, const juce::File& folder) {
    std::array<juce::File, gatherer::protocol::NUM_SLOTS> by_slot;
    for (const auto& entry : juce::RangedDirectoryIterator(folder, false, "slot*.wav",
                                                            juce::File::findFiles)) {
        const auto f    = entry.getFile();
        const auto name = f.getFileNameWithoutExtension();
        // Names are "slotNN_..." — pull NN.
        const auto under = name.indexOfChar('_');
        if (under < 5) continue;
        const auto idx = name.substring(4, under).getIntValue();
        if (idx < 0 || idx >= static_cast<int>(by_slot.size())) continue;
        // Skip normalized siblings.
        if (name.endsWith("_normalized")) continue;
        by_slot[static_cast<std::size_t>(idx)] = f;
    }
    for (std::size_t s = 0; s < by_slot.size(); ++s) {
        const int slot = static_cast<int>(s);
        if (auto* tn = p.getThumbnail(slot)) {
            tn->reset(0, 0);
            if (by_slot[s] != juce::File{}) {
                tn->setSource(new juce::FileInputSource(by_slot[s]));
            }
        }
        p.setLastRecordingForSlot(slot, by_slot[s]);
    }
}
}  // namespace

SessionManager::SessionManager(HubProcessor& processor) : processor_(processor) {}

juce::File SessionManager::defaultParentFolder() {
    return juce::File::getSpecialLocation(juce::File::userDocumentsDirectory)
            .getChildFile("Gatherer Recordings");
}

juce::String SessionManager::currentName() const noexcept {
    return hasSession() ? current_folder_.getFileName() : juce::String{};
}

void SessionManager::newSession() {
    auto base = defaultParentFolder();
    base.createDirectory();

    auto folder = base.getChildFile(timestampNow());
    // Disambiguate if two clicks land in the same second.
    int suffix = 1;
    while (folder.exists()) {
        folder = base.getChildFile(timestampNow() + "_" + juce::String(suffix++));
    }
    folder.createDirectory();

    current_folder_ = folder;

    // Fresh playback / thumbnail state — previous session's recordings and
    // sources are dropped. Mix params stay so the user's setup carries over.
    processor_.playback().clearAll();
    processor_.resetGrid();
    for (int i = 0; i < static_cast<int>(gatherer::protocol::NUM_SLOTS); ++i) {
        processor_.setLastRecordingForSlot(i, juce::File{});
        if (auto* tn = processor_.getThumbnail(i)) tn->reset(0, 0);
    }
    save();
    notify();
}

bool SessionManager::openSession(const juce::File& folder) {
    if (!folder.isDirectory()) return false;

    current_folder_ = folder;

    // Load manifest if present; otherwise just adopt the folder and use defaults.
    const auto manifest = folder.getChildFile(kManifestName);
    if (manifest.existsAsFile()) {
        const auto txt = manifest.loadFileAsString();
        const auto v   = juce::JSON::parse(txt);
        if (v.isObject()) applyManifestVar(v);
    }

    // Rebuild playback sources + thumbnails from whatever's on disk.
    processor_.refreshPlaybackSources();
    rebuildThumbnailsFromFolder(processor_, folder);
    processor_.recomputeSessionLayout();

    notify();
    return true;
}

bool SessionManager::save() {
    if (!hasSession()) return false;
    current_folder_.createDirectory();

    const auto v   = buildManifestVar();
    const auto txt = juce::JSON::toString(v, /*allOnOneLine*/ false);
    const auto out = current_folder_.getChildFile(kManifestName);
    return out.replaceWithText(txt);
}

juce::File SessionManager::ensureFolderForRecording() {
    if (!hasSession()) newSession();
    return current_folder_;
}

void SessionManager::setCurrentFolderRaw(juce::File f) {
    current_folder_ = std::move(f);
    notify();
}

juce::String SessionManager::serializeForPluginState() const {
    return current_folder_.getFullPathName();
}

void SessionManager::restoreFromPluginState(const juce::String& s) {
    if (s.isEmpty()) return;
    const juce::File f(s);
    if (!f.isDirectory()) return;
    openSession(f);
}

juce::var SessionManager::buildManifestVar() const {
    auto* root = new juce::DynamicObject();
    root->setProperty("version", kManifestVersion);
    root->setProperty("name",    currentName());
    root->setProperty("created",
                      juce::Time::getCurrentTime().toISO8601(true));

    auto* global = new juce::DynamicObject();
    global->setProperty("target_lufs",         processor_.getTargetLufs());
    global->setProperty("include_track_input", processor_.isIncludeTrackInput());
    root->setProperty("global", juce::var(global));

    juce::Array<juce::var> order_arr;
    for (int v : processor_.getDisplayOrder()) order_arr.add(v);
    root->setProperty("display_order", order_arr);

    if (const auto g = processor_.getCurrentGridInfo(); g.captured) {
        auto* grid = new juce::DynamicObject();
        grid->setProperty("bpm",              g.bpm);
        grid->setProperty("time_sig_num",     g.time_sig_num);
        grid->setProperty("time_sig_den",     g.time_sig_den);
        // Reference start (first slot to capture) — kept for backward compat.
        grid->setProperty("start_in_seconds", g.start_in_seconds);
        grid->setProperty("start_in_beats",   g.start_in_beats);

        juce::Array<juce::var> per;
        for (int i = 0; i < static_cast<int>(gatherer::protocol::NUM_SLOTS); ++i) {
            const auto sg = processor_.getSlotGridInfo(i);
            if (!sg.captured) continue;
            auto* o = new juce::DynamicObject();
            o->setProperty("slot",             i);
            o->setProperty("start_in_seconds", sg.start_in_seconds);
            o->setProperty("start_in_beats",   sg.start_in_beats);
            per.add(juce::var(o));
        }
        grid->setProperty("per_slot", per);
        root->setProperty("grid", juce::var(grid));
    }

    juce::Array<juce::var> slots;
    for (int i = 0; i < static_cast<int>(gatherer::protocol::NUM_SLOTS); ++i) {
        auto* s = new juce::DynamicObject();
        s->setProperty("slot",         i);
        auto* mix = new juce::DynamicObject();
        mix->setProperty("mute",        processor_.getMute(i));
        mix->setProperty("solo",        processor_.getSolo(i));
        mix->setProperty("gain_db",     processor_.getGainDb(i));
        mix->setProperty("norm_db",     processor_.getNormalizeDb(i));
        mix->setProperty("target_lufs", processor_.getSlotTargetLufs(i));
        mix->setProperty("record_arm",  processor_.getRecordArm(i));
        s->setProperty("mix", juce::var(mix));

        // PDC override (manually set by the user via the LayerRow's PDC field).
        // The auto-measured value is *not* persisted — it's re-measured every
        // session, and persisting it would mask whether the calibrator is
        // currently working. Only the override is sticky.
        const auto pdc_ov = processor_.pdcDOverride(i);
        if (pdc_ov != HubProcessor::kPdcUnknown) {
            s->setProperty("pdc_override_samples", static_cast<juce::int64>(pdc_ov));
        }

        const auto rec = processor_.getLastRecordingForSlot(i);
        if (rec != juce::File{}) {
            s->setProperty("recording", rec.getFileName());
            const auto norm = rec.getSiblingFile(
                rec.getFileNameWithoutExtension() + "_normalized.wav");
            if (norm.existsAsFile()) {
                s->setProperty("normalized", norm.getFileName());
            }
        }
        slots.add(juce::var(s));
    }
    root->setProperty("slots", slots);
    return juce::var(root);
}

bool SessionManager::applyManifestVar(const juce::var& v) {
    if (!v.isObject()) return false;

    if (auto global = v["global"]; global.isObject()) {
        if (global.hasProperty("target_lufs")) {
            processor_.setTargetLufs(static_cast<float>(static_cast<double>(global["target_lufs"])));
        }
        if (global.hasProperty("include_track_input")) {
            processor_.setIncludeTrackInput(static_cast<bool>(global["include_track_input"]));
        }
    }

    if (auto order = v["display_order"]; order.isArray()
        && static_cast<std::size_t>(order.size()) == gatherer::protocol::NUM_SLOTS) {
        std::array<int, gatherer::protocol::NUM_SLOTS> arr{};
        for (int i = 0; i < static_cast<int>(arr.size()); ++i) {
            arr[static_cast<std::size_t>(i)] = static_cast<int>(order[i]);
        }
        processor_.setDisplayOrder(arr);
    }

    if (auto grid = v["grid"]; grid.isObject()) {
        HubProcessor::GridInfo gi;
        gi.captured         = true;
        gi.bpm              = static_cast<double>(grid["bpm"]);
        gi.time_sig_num     = static_cast<int>(grid["time_sig_num"]);
        gi.time_sig_den     = static_cast<int>(grid["time_sig_den"]);
        gi.start_in_seconds = static_cast<double>(grid["start_in_seconds"]);
        gi.start_in_beats   = static_cast<double>(grid["start_in_beats"]);
        processor_.setCurrentGridInfo(gi);

        if (auto per = grid["per_slot"]; per.isArray()) {
            for (const auto& sv : *per.getArray()) {
                if (!sv.isObject()) continue;
                const int idx = static_cast<int>(sv["slot"]);
                HubProcessor::SlotGridInfo sg;
                sg.captured         = true;
                sg.start_in_seconds = static_cast<double>(sv["start_in_seconds"]);
                sg.start_in_beats   = static_cast<double>(sv["start_in_beats"]);
                processor_.setSlotGridInfo(idx, sg);
            }
        } else {
            // Old-format manifest with only session-level start. Apply it to
            // every slot that has a recording, so lanes still draw a grid.
            HubProcessor::SlotGridInfo sg;
            sg.captured         = true;
            sg.start_in_seconds = gi.start_in_seconds;
            sg.start_in_beats   = gi.start_in_beats;
            for (int i = 0; i < static_cast<int>(gatherer::protocol::NUM_SLOTS); ++i) {
                if (processor_.getLastRecordingForSlot(i) != juce::File{}) {
                    processor_.setSlotGridInfo(i, sg);
                }
            }
        }
    } else {
        processor_.resetGrid();
    }

    if (auto slots = v["slots"]; slots.isArray()) {
        for (const auto& s_var : *slots.getArray()) {
            if (!s_var.isObject()) continue;
            const int idx = static_cast<int>(s_var["slot"]);
            if (idx < 0 || idx >= static_cast<int>(gatherer::protocol::NUM_SLOTS)) continue;

            if (auto mix = s_var["mix"]; mix.isObject()) {
                if (mix.hasProperty("mute"))
                    processor_.setMute(idx, static_cast<bool>(mix["mute"]));
                if (mix.hasProperty("solo"))
                    processor_.setSolo(idx, static_cast<bool>(mix["solo"]));
                if (mix.hasProperty("gain_db"))
                    processor_.setGainDb(idx, static_cast<float>(static_cast<double>(mix["gain_db"])));
                if (mix.hasProperty("norm_db"))
                    processor_.setNormalizeDb(idx, static_cast<float>(static_cast<double>(mix["norm_db"])));
                if (mix.hasProperty("target_lufs"))
                    processor_.setSlotTargetLufs(idx, static_cast<float>(static_cast<double>(mix["target_lufs"])));
                if (mix.hasProperty("record_arm"))
                    processor_.setRecordArm(idx, static_cast<bool>(mix["record_arm"]));
            }

            if (s_var.hasProperty("pdc_override_samples")) {
                processor_.setPdcDOverride(
                    idx, static_cast<std::int64_t>(static_cast<juce::int64>(s_var["pdc_override_samples"])));
            } else {
                processor_.clearPdcDOverride(idx);
            }
        }
    }

    return true;
}

}  // namespace gatherer::session

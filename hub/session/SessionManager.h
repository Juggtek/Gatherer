#pragma once

#include <JuceHeader.h>

#include <functional>

class HubProcessor;

namespace gatherer::session {

// Owns the "current session folder" and orchestrates manifest read/write.
// A session is a directory under ~/Documents/Gatherer Recordings/ containing
// the recorded WAVs plus a manifest.json snapshot of the hub's state at the
// time of the last save.
//
// Lifecycle:
//   - Plugin construction: SessionManager has no current folder until either
//     newSession(), openSession(), or startRecording() runs (the latter creates
//     a folder implicitly on first record).
//   - Auto-save fires on stopRecording, deleteRecording, and explicit Save.
//   - The current folder path lives in plugin state so the host's project save
//     resumes the same session on reload.
class SessionManager {
public:
    explicit SessionManager(HubProcessor& processor);

    // Current session folder (empty until a session is started).
    juce::File   currentFolder() const noexcept { return current_folder_; }
    juce::String currentName()   const noexcept;
    bool         hasSession()    const noexcept { return current_folder_ != juce::File{}; }

    // Default parent for all sessions (~/Documents/Gatherer Recordings).
    static juce::File defaultParentFolder();

    // Create a fresh timestamped session folder. Existing recordings + the
    // playback engine are cleared; mix params stay so the next record uses
    // whatever the user already set up.
    void newSession();

    // Open an existing folder. If it has a manifest.json, applies it to the
    // processor (mix state, target LUFS, etc.). Reloads playback sources +
    // thumbnails from the WAVs found in the folder. Returns true on success.
    bool openSession(const juce::File& folder);

    // Write current state to {folder}/manifest.json. Quiet no-op if no session.
    bool save();

    // Convenience: called by HubProcessor after stop-recording, delete-record,
    // and similar mutations. Same as save() but silent.
    void autoSave() { save(); }

    // Used by HubProcessor::startRecording: ensure we have a folder to record
    // into. If none, creates one (acts like newSession). Returns the folder.
    juce::File ensureFolderForRecording();

    // Persisted in plugin state so the host project reload resumes the session.
    void          setCurrentFolderRaw(juce::File f);
    juce::String  serializeForPluginState() const;
    void          restoreFromPluginState(const juce::String& serialized);

    std::function<void()> onChange;

private:
    void notify() { if (onChange) onChange(); }

    juce::var   buildManifestVar() const;
    bool        applyManifestVar(const juce::var& v);

    HubProcessor& processor_;
    juce::File    current_folder_;
};

}  // namespace gatherer::session

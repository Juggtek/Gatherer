#pragma once

#include <JuceHeader.h>

#include "Command.h"

class HubProcessor;

namespace gatherer::undo {

// Per-slot bool toggles (mute / solo / record-arm). No coalescing — every click
// is a discrete user intent.
class SetMuteCommand : public Command {
public:
    SetMuteCommand(HubProcessor& p, int slot, bool new_v, bool old_v)
        : p_(&p), slot_(slot), new_v_(new_v), old_v_(old_v) {}
    void         apply()    override;
    void         revert()   override;
    juce::String describe() const override { return "Mute"; }
private:
    HubProcessor* p_; int slot_; bool new_v_, old_v_;
};

class SetSoloCommand : public Command {
public:
    SetSoloCommand(HubProcessor& p, int slot, bool new_v, bool old_v)
        : p_(&p), slot_(slot), new_v_(new_v), old_v_(old_v) {}
    void         apply()    override;
    void         revert()   override;
    juce::String describe() const override { return "Solo"; }
private:
    HubProcessor* p_; int slot_; bool new_v_, old_v_;
};

class SetRecordArmCommand : public Command {
public:
    SetRecordArmCommand(HubProcessor& p, int slot, bool new_v, bool old_v)
        : p_(&p), slot_(slot), new_v_(new_v), old_v_(old_v) {}
    void         apply()    override;
    void         revert()   override;
    juce::String describe() const override { return "Arm"; }
private:
    HubProcessor* p_; int slot_; bool new_v_, old_v_;
};

// Volume fader and normalize-stage gain — both float, both coalesce.
class SetGainDbCommand : public Command {
public:
    SetGainDbCommand(HubProcessor& p, int slot, float new_db, float old_db)
        : p_(&p), slot_(slot), new_db_(new_db), old_db_(old_db) {}
    void         apply()    override;
    void         revert()   override;
    juce::String describe() const override { return "Volume"; }
    bool         coalescesWith(const Command& previous) const override;
    void         coalesceFrom(Command&& previous)             override;
private:
    HubProcessor* p_; int slot_; float new_db_, old_db_;
};

class SetNormalizeDbCommand : public Command {
public:
    SetNormalizeDbCommand(HubProcessor& p, int slot, float new_db, float old_db)
        : p_(&p), slot_(slot), new_db_(new_db), old_db_(old_db) {}
    void         apply()    override;
    void         revert()   override;
    juce::String describe() const override { return "Normalize"; }
    bool         coalescesWith(const Command& previous) const override;
    void         coalesceFrom(Command&& previous)             override;
private:
    HubProcessor* p_; int slot_; float new_db_, old_db_;
};

// Target LUFS — per-slot and global. Coalesce so rapid edits (deleting digits
// then retyping) collapse into one undo step.
class SetSlotTargetLufsCommand : public Command {
public:
    SetSlotTargetLufsCommand(HubProcessor& p, int slot, float new_v, float old_v)
        : p_(&p), slot_(slot), new_v_(new_v), old_v_(old_v) {}
    void         apply()    override;
    void         revert()   override;
    juce::String describe() const override { return "Target LUFS"; }
    bool         coalescesWith(const Command& previous) const override;
    void         coalesceFrom(Command&& previous)             override;
private:
    HubProcessor* p_; int slot_; float new_v_, old_v_;
};

class SetGlobalTargetLufsCommand : public Command {
public:
    SetGlobalTargetLufsCommand(HubProcessor& p, float new_v, float old_v)
        : p_(&p), new_v_(new_v), old_v_(old_v) {}
    void         apply()    override;
    void         revert()   override;
    juce::String describe() const override { return "Global Target LUFS"; }
    bool         coalescesWith(const Command& previous) const override;
    void         coalesceFrom(Command&& previous)             override;
private:
    HubProcessor* p_; float new_v_, old_v_;
};

}  // namespace gatherer::undo

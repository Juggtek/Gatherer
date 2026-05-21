#include "MixCommands.h"

#include "../PluginProcessor.h"

namespace gatherer::undo {

namespace {
constexpr int kCoalesceWindowMs = 500;
}  // namespace

void SetMuteCommand::apply()  { p_->setMute(slot_, new_v_); }
void SetMuteCommand::revert() { p_->setMute(slot_, old_v_); }

void SetSoloCommand::apply()  { p_->setSolo(slot_, new_v_); }
void SetSoloCommand::revert() { p_->setSolo(slot_, old_v_); }

void SetRecordArmCommand::apply()  { p_->setRecordArm(slot_, new_v_); }
void SetRecordArmCommand::revert() { p_->setRecordArm(slot_, old_v_); }

void SetGainDbCommand::apply()  { p_->setGainDb(slot_, new_db_); }
void SetGainDbCommand::revert() { p_->setGainDb(slot_, old_db_); }
bool SetGainDbCommand::coalescesWith(const Command& previous) const {
    const auto* o = dynamic_cast<const SetGainDbCommand*>(&previous);
    return o != nullptr && o->slot_ == slot_ && isWithin(previous, kCoalesceWindowMs);
}
void SetGainDbCommand::coalesceFrom(Command&& previous) {
    auto& o = static_cast<SetGainDbCommand&>(previous);
    old_db_ = o.old_db_;
}

void SetNormalizeDbCommand::apply()  { p_->setNormalizeDb(slot_, new_db_); }
void SetNormalizeDbCommand::revert() { p_->setNormalizeDb(slot_, old_db_); }
bool SetNormalizeDbCommand::coalescesWith(const Command& previous) const {
    const auto* o = dynamic_cast<const SetNormalizeDbCommand*>(&previous);
    return o != nullptr && o->slot_ == slot_ && isWithin(previous, kCoalesceWindowMs);
}
void SetNormalizeDbCommand::coalesceFrom(Command&& previous) {
    auto& o = static_cast<SetNormalizeDbCommand&>(previous);
    old_db_ = o.old_db_;
}

void SetSlotTargetLufsCommand::apply()  { p_->setSlotTargetLufs(slot_, new_v_); }
void SetSlotTargetLufsCommand::revert() { p_->setSlotTargetLufs(slot_, old_v_); }
bool SetSlotTargetLufsCommand::coalescesWith(const Command& previous) const {
    const auto* o = dynamic_cast<const SetSlotTargetLufsCommand*>(&previous);
    return o != nullptr && o->slot_ == slot_ && isWithin(previous, kCoalesceWindowMs);
}
void SetSlotTargetLufsCommand::coalesceFrom(Command&& previous) {
    auto& o = static_cast<SetSlotTargetLufsCommand&>(previous);
    old_v_ = o.old_v_;
}

void SetGlobalTargetLufsCommand::apply()  { p_->setTargetLufs(new_v_); }
void SetGlobalTargetLufsCommand::revert() { p_->setTargetLufs(old_v_); }
bool SetGlobalTargetLufsCommand::coalescesWith(const Command& previous) const {
    const auto* o = dynamic_cast<const SetGlobalTargetLufsCommand*>(&previous);
    return o != nullptr && isWithin(previous, kCoalesceWindowMs);
}
void SetGlobalTargetLufsCommand::coalesceFrom(Command&& previous) {
    auto& o = static_cast<SetGlobalTargetLufsCommand&>(previous);
    old_v_ = o.old_v_;
}

}  // namespace gatherer::undo

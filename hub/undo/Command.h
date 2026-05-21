#pragma once

#include <JuceHeader.h>

#include <chrono>

namespace gatherer::undo {

// Base interface for an undoable user action.
//
// Lifecycle: the command is constructed at the call site with whatever state it
// needs to apply/revert (typically: pointer to the processor, slot index, old
// value, new value). The CommandStack calls apply() immediately on execute(),
// then revert() / apply() on subsequent undo() / redo().
//
// Coalescing: two commands "coalesce" when a new action arrives that the stack
// can merge with the previous undo step. The canonical case is a slider drag —
// dozens of micro-changes that should appear as one undo step. coalescesWith()
// inspects the candidate (which is at the top of done_) and returns true to
// merge. coalesceFrom() runs on the *new* command and absorbs whatever state it
// needs from the old one (typically: the earliest old_value).
class Command {
public:
    Command() : created_at_(std::chrono::steady_clock::now()) {}
    virtual ~Command() = default;

    virtual void apply()  = 0;
    virtual void revert() = 0;

    // One-line label for the Undo/Redo button tooltip / future history panel.
    virtual juce::String describe() const = 0;

    virtual bool coalescesWith(const Command& /*previous*/) const { return false; }
    virtual void coalesceFrom(Command&& /*previous*/)             {}

    std::chrono::steady_clock::time_point createdAt() const noexcept { return created_at_; }

protected:
    // Helper for coalescing: are we within `window_ms` of `previous`?
    //
    // Note: each new command keeps its own `created_at_` (we deliberately do NOT
    // inherit the previous timestamp on coalesce). That way the window rolls
    // forward with sustained activity — a slider drag that lasts several seconds
    // still collapses into one undo step because each move event refreshes the
    // 500 ms horizon.
    bool isWithin(const Command& previous, int window_ms) const {
        const auto delta = std::chrono::duration_cast<std::chrono::milliseconds>(
            created_at_ - previous.created_at_).count();
        return delta >= 0 && delta < window_ms;
    }

private:
    std::chrono::steady_clock::time_point created_at_;
};

}  // namespace gatherer::undo

#pragma once

#include <JuceHeader.h>

#include <memory>
#include <vector>

#include "Command.h"

namespace gatherer::undo {

// Single-threaded command stack. Owned by the HubProcessor; mutated only from
// the message thread (button clicks, slider changes, keyboard shortcuts).
//
// execute() runs the command immediately, clears the redo stack, and pushes
// onto done_. If the command coalesces with the top of done_ (e.g. slider drag
// within 500ms), the top is replaced rather than appended.
//
// Future: serialize() / deserialize() once the session manifest exists.
class CommandStack {
public:
    static constexpr std::size_t kMaxHistory = 256;

    void execute(std::unique_ptr<Command> cmd) {
        if (!cmd) return;
        cmd->apply();
        undone_.clear();
        if (!done_.empty() && cmd->coalescesWith(*done_.back())) {
            cmd->coalesceFrom(std::move(*done_.back()));
            done_.pop_back();
        }
        done_.push_back(std::move(cmd));
        trimHistory();
        notify();
    }

    bool canUndo() const noexcept { return !done_.empty(); }
    bool canRedo() const noexcept { return !undone_.empty(); }

    void undo() {
        if (done_.empty()) return;
        auto cmd = std::move(done_.back());
        done_.pop_back();
        cmd->revert();
        undone_.push_back(std::move(cmd));
        notify();
    }

    void redo() {
        if (undone_.empty()) return;
        auto cmd = std::move(undone_.back());
        undone_.pop_back();
        cmd->apply();
        done_.push_back(std::move(cmd));
        notify();
    }

    void clear() {
        done_  .clear();
        undone_.clear();
        notify();
    }

    juce::String topUndoLabel() const {
        return done_.empty() ? juce::String{} : done_.back()->describe();
    }
    juce::String topRedoLabel() const {
        return undone_.empty() ? juce::String{} : undone_.back()->describe();
    }

    // Fired after any mutation of the stack. The editor uses this to refresh
    // the Undo/Redo button enable state and tooltips.
    std::function<void()> onChange;

private:
    void trimHistory() {
        while (done_.size() > kMaxHistory) {
            done_.erase(done_.begin());
        }
    }
    void notify() { if (onChange) onChange(); }

    std::vector<std::unique_ptr<Command>> done_;
    std::vector<std::unique_ptr<Command>> undone_;
};

// Convenience: bundle several sub-commands into one undo step. Used for
// "Normalize All" — each per-slot change is a SetNormalizeDb, the whole batch
// is one undo.
class CompositeCommand : public Command {
public:
    CompositeCommand(juce::String label,
                     std::vector<std::unique_ptr<Command>> children)
        : label_(std::move(label)), children_(std::move(children)) {}

    void apply() override {
        for (auto& c : children_) c->apply();
    }
    void revert() override {
        // Reverse order so dependencies unwind cleanly.
        for (auto it = children_.rbegin(); it != children_.rend(); ++it) {
            (*it)->revert();
        }
    }
    juce::String describe() const override { return label_; }

private:
    juce::String                          label_;
    std::vector<std::unique_ptr<Command>> children_;
};

}  // namespace gatherer::undo

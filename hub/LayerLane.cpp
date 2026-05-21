#include "LayerLane.h"

LayerLane::LayerLane() {
    setOpaque(false);
}

LayerLane::~LayerLane() {
    setThumbnail(nullptr);
}

void LayerLane::setThumbnail(juce::AudioThumbnail* t) {
    if (thumbnail_ == t) return;
    if (thumbnail_ != nullptr) thumbnail_->removeChangeListener(this);
    thumbnail_ = t;
    if (thumbnail_ != nullptr) thumbnail_->addChangeListener(this);
    repaint();
}

void LayerLane::changeListenerCallback(juce::ChangeBroadcaster*) {
    repaint();
}

void LayerLane::mouseDown(const juce::MouseEvent& e) {
    if (e.mods.isPopupMenu()) {
        juce::PopupMenu m;
        m.addItem(1, "Delete recording", has_recording_);
        m.showMenuAsync(juce::PopupMenu::Options{}.withTargetComponent(this),
                        [this](int result) {
                            if (result == 1 && onDeleteRecording) onDeleteRecording();
                        });
        return;
    }
    // Left-click → seek.
    if (onSeekFraction && getWidth() > 0) {
        const double f = juce::jlimit(0.0, 1.0,
            static_cast<double>(e.x) / static_cast<double>(getWidth()));
        onSeekFraction(f);
    }
}

void LayerLane::mouseDrag(const juce::MouseEvent& e) {
    if (e.mods.isPopupMenu()) return;
    if (onSeekFraction && getWidth() > 0) {
        const double f = juce::jlimit(0.0, 1.0,
            static_cast<double>(e.x) / static_cast<double>(getWidth()));
        onSeekFraction(f);
    }
}

void LayerLane::setPlayheadFraction(double f) {
    f = juce::jlimit(0.0, 1.0, f);
    if (std::abs(f - playhead_fraction_) < 1e-4) return;
    playhead_fraction_ = f;
    repaint();
}

void LayerLane::setGrid(double bpm, int time_sig_num, double session_start_in_beats,
                        double session_seconds) {
    if (std::abs(bpm - grid_bpm_) < 1e-3
        && time_sig_num == grid_time_sig_num_
        && std::abs(session_start_in_beats - grid_start_beats_) < 1e-3
        && std::abs(session_seconds - grid_session_secs_) < 1e-3) {
        return;
    }
    grid_bpm_          = bpm;
    grid_time_sig_num_ = time_sig_num;
    grid_start_beats_  = session_start_in_beats;
    grid_session_secs_ = session_seconds;
    repaint();
}

void LayerLane::setSlotLayout(double slot_offset_seconds, double slot_length_seconds) {
    if (std::abs(slot_offset_seconds - slot_offset_secs_) < 1e-4
        && std::abs(slot_length_seconds - slot_length_secs_) < 1e-4) {
        return;
    }
    slot_offset_secs_ = slot_offset_seconds;
    slot_length_secs_ = slot_length_seconds;
    repaint();
}

void LayerLane::paint(juce::Graphics& g) {
    auto bounds = getLocalBounds();

    // Lane background
    g.setColour(juce::Colours::black.withAlpha(0.35f));
    g.fillRoundedRectangle(bounds.toFloat(), 3.0f);

    // Beat grid — drawn under the waveform so the thumbnail stays the dominant
    // visual. Skip when too dense to be useful (less than ~3px per beat).
    //
    // Phase-aligned to the DAW: the recording's sample 0 corresponds to DAW
    // PPQ position `grid_start_beats_`. The first beat line we draw is the
    // next whole DAW beat at or after sample 0 — offset by the fractional
    // part of that PPQ. Downbeats are determined by the *global* beat number
    // (which bar of the project we'd be in), not lane-local.
    if (grid_bpm_ > 0.0 && grid_session_secs_ > 0.0) {
        const double beat_sec    = 60.0 / grid_bpm_;
        const double total_beats = grid_session_secs_ / beat_sec;
        const double px_per_beat = static_cast<double>(bounds.getWidth()) / total_beats;
        if (px_per_beat >= 3.0) {
            constexpr double kBeatTolerance = 0.01;  // treat sub-1% as on-beat
            const double start = grid_start_beats_;
            const int    first_global_beat = static_cast<int>(std::ceil(start - kBeatTolerance));
            double       first_offset_sec  = (first_global_beat - start) * beat_sec;
            if (first_offset_sec < 0.0) first_offset_sec = 0.0;

            // Decide whether to draw bar.beat labels. Skip when sub-beat space
            // is too tight to be legible (less than ~30 px per beat).
            const bool draw_labels = px_per_beat >= 30.0;
            g.setFont(juce::FontOptions(9.0f));

            for (int i = 0; ; ++i) {
                const double t = first_offset_sec + i * beat_sec;
                if (t > grid_session_secs_) break;
                const double frac = t / grid_session_secs_;
                const int x = bounds.getX()
                            + juce::roundToInt(static_cast<double>(bounds.getWidth()) * frac);
                const int global_beat = first_global_beat + i;
                const bool downbeat = (grid_time_sig_num_ > 0)
                                    && (global_beat % grid_time_sig_num_ == 0);
                g.setColour(downbeat ? juce::Colours::white.withAlpha(0.28f)
                                     : juce::Colours::white.withAlpha(0.10f));
                g.fillRect(x, bounds.getY(), 1, bounds.getHeight());

                if (draw_labels && grid_time_sig_num_ > 0) {
                    // Floor division for negative global_beat — keeps labels
                    // sensible if a recording's grid offset ended up before the
                    // project start (rare now that we defer recording until
                    // play, but still defensively correct).
                    const int n = grid_time_sig_num_;
                    int bar_idx, beat_idx;
                    if (global_beat >= 0) {
                        bar_idx  = global_beat / n;
                        beat_idx = global_beat % n;
                    } else {
                        const int abs_below = -global_beat;
                        bar_idx  = -((abs_below + n - 1) / n);
                        beat_idx = ((global_beat % n) + n) % n;
                    }
                    const juce::String label = juce::String(bar_idx + 1) + "."
                                             + juce::String(beat_idx + 1);
                    g.setColour(downbeat
                        ? juce::Colours::white.withAlpha(0.55f)
                        : juce::Colours::white.withAlpha(0.30f));
                    g.drawText(label,
                               x + 2, bounds.getY() + 1,
                               40, 11,
                               juce::Justification::topLeft);
                }
            }
        }
    }

    const bool has_thumb = thumbnail_ != nullptr && thumbnail_->getTotalLength() > 0.0;
    if (!has_thumb) {
        // Empty state — faint placeholder line through the middle.
        g.setColour(juce::Colours::white.withAlpha(0.12f));
        const int y = bounds.getCentreY();
        g.drawHorizontalLine(y, static_cast<float>(bounds.getX()),
                                static_cast<float>(bounds.getRight()));
    } else if (grid_session_secs_ > 0.0) {
        // Draw the thumbnail at its position within the session timeline. Audio
        // starts at slot_offset_secs_ (in session-relative seconds) and runs
        // for thumbnail->getTotalLength() seconds.
        const double thumb_len = thumbnail_->getTotalLength();
        const double frac_start = slot_offset_secs_ / grid_session_secs_;
        const double frac_end   = (slot_offset_secs_ + thumb_len) / grid_session_secs_;
        const int x0 = bounds.getX()
                     + juce::roundToInt(static_cast<double>(bounds.getWidth())
                                         * juce::jlimit(0.0, 1.0, frac_start));
        const int x1 = bounds.getX()
                     + juce::roundToInt(static_cast<double>(bounds.getWidth())
                                         * juce::jlimit(0.0, 1.0, frac_end));
        const juce::Rectangle<int> thumb_bounds(x0, bounds.getY() + 2,
                                                 std::max(1, x1 - x0),
                                                 bounds.getHeight() - 4);
        g.setColour(juce::Colours::white.withAlpha(0.75f));
        thumbnail_->drawChannels(g, thumb_bounds,
                                  /*startTimeSeconds*/ 0.0,
                                  /*endTimeSeconds*/   thumb_len,
                                  /*vertical zoom*/    1.0f);
    } else {
        // Fallback (no session info) — fill the lane like before.
        g.setColour(juce::Colours::white.withAlpha(0.75f));
        thumbnail_->drawChannels(g, bounds.reduced(2),
                                  0.0, thumbnail_->getTotalLength(), 1.0f);
    }

    // Playhead — always drawn (even on empty lanes) so the global position is
    // visible across the whole layer stack.
    const int x = bounds.getX()
                + juce::roundToInt(static_cast<double>(bounds.getWidth())
                                    * playhead_fraction_);
    g.setColour(juce::Colours::white.withAlpha(0.9f));
    g.fillRect(x, bounds.getY(), 2, bounds.getHeight());
}

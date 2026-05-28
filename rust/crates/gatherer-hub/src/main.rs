//! Gatherer Hub — standalone Rust mixer. cpal direct + iced UI.
//!
//! Gathers audio that a host DAW routes out to a (virtual or hardware)
//! multichannel device: each device input channel pair is one "source".
//! Because the DAW applies full plugin-delay-compensation before the
//! audio leaves, the sources arrive already time-aligned — no in-host
//! plugin, no shared-memory transport, no topology constraints. See the
//! migration plan for the PDC reasoning.

mod adaptive;
mod app;
mod audio;
mod export;
mod measurement;
mod midi;
mod navigator;
mod params;
mod playback;
mod recording;
mod sequencer;
mod session;
mod template;

use app::State;

fn main() -> iced::Result {
    iced::application("Gatherer Hub", State::update, State::view)
        .subscription(State::subscription)
        .antialiasing(true)
        .window(iced::window::Settings {
            size: iced::Size::new(1700.0, 1000.0),
            min_size: Some(iced::Size::new(1100.0, 700.0)),
            ..Default::default()
        })
        .run_with(|| (State::new(), iced::Task::none()))
}

//! Gatherer Hub — standalone Rust mixer. cpal direct + iced UI.
//!
//! Gathers audio that a host DAW routes out to a (virtual or hardware)
//! multichannel device: each device input channel pair is one "source".
//! Because the DAW applies full plugin-delay-compensation before the
//! audio leaves, the sources arrive already time-aligned — no in-host
//! plugin, no shared-memory transport, no topology constraints. See the
//! migration plan for the PDC reasoning.

mod app;
mod audio;
mod params;
mod recording;

use app::State;

fn main() -> iced::Result {
    iced::application("Gatherer Hub", State::update, State::view)
        .subscription(State::subscription)
        .antialiasing(true)
        .window(iced::window::Settings {
            size: iced::Size::new(900.0, 600.0),
            min_size: Some(iced::Size::new(600.0, 400.0)),
            ..Default::default()
        })
        .run_with(|| (State::new(), iced::Task::none()))
}

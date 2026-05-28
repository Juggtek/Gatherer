//! Navigator — the project-tree structural model + edit operations,
//! ported from the engine team's reference Navigator plugin (see
//! `rust/docs/NAVIGATOR_PORT.md`). Pure data + structural mutations; no
//! audio, no UI here.
//!
//! The full mutation/query API lands here ahead of its UI consumers
//! (Phase B0 wires the section selector, B1 the full editor), so the
//! module allows dead code until those callers exist.
#![allow(dead_code)]

pub mod model;
pub mod ops;
pub mod queries;

// Convenience re-exports — the full public surface. Some names aren't
// consumed until the UI phases, so allow unused on the re-export.
#[allow(unused_imports)]
pub use model::{
    Asset, AssetMeta, AssetType, ChannelStrip, ClipSource, Module, Page, Pattern, Project, Region,
    Section, SectionKind,
};

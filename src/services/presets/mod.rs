//! Project setup presets — pre-cooked Copy Patterns / Ignore Patterns /
//! Post-Create Commands for the most common languages and frameworks.
//!
//! The catalog lives in [`catalog`]; the per-preset signature matcher used by
//! the menu's auto-detection logic lives in [`detect`].

pub mod catalog;
pub mod detect;

pub use catalog::{catalog, find_by_id, Preset, PresetId};
pub use detect::detect;

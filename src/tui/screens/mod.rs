//! Screen-level renderers. Each screen module is a pure draw function plus,
//! eventually, a small piece of state owned by `App`.

pub mod create;
pub mod dashboard;
pub mod delete;
pub mod error;
pub mod loading;
pub mod menu;
pub mod merge_pr;
pub mod settings;
pub mod setup;

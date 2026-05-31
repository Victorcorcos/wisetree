//! Screen-level renderers. Each screen module is a pure draw function plus,
//! eventually, a small piece of state owned by `App`.

pub mod ai_model_picker;
pub mod cache;
pub mod create;
pub mod dashboard;
pub mod delete;
pub mod error;
pub mod fill_pr;
pub mod loading;
pub mod menu;
pub mod merge_pr;
pub mod settings;
pub mod setup;
pub mod setup_project;
pub mod update_branch;
pub mod update_pr;

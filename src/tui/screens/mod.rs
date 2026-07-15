//! Screen-level renderers. Each screen module is a pure draw function plus,
//! eventually, a small piece of state owned by `App`.

pub mod ai_model_picker;
pub mod bugkill_pr;
pub mod cache;
pub mod create;
pub mod dashboard;
pub mod delete;
pub mod enrich_pr;
pub mod error;
pub mod fix_pr;
pub mod loading;
pub mod menu;
pub mod merge_pr;
pub mod review_pr;
pub mod settings;
pub mod setup;
pub mod setup_project;
pub mod update_branch;
pub mod update_pr;

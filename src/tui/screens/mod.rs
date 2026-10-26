//! Screen-level renderers. Each screen module is a pure draw function plus,
//! eventually, a small piece of state owned by `App`.

pub mod cache;
pub mod create;
pub mod dashboard;
pub mod dashboard_filter;
pub mod delete;
pub mod error;
pub mod global_accounts;
pub mod loading;
pub mod menu;
pub mod merge_pr;
pub mod settings;
pub mod setup;
pub mod setup_project;

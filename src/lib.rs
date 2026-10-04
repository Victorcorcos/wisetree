//! Wisetree — interactive Git worktree manager.

pub mod cli;
pub mod config;
pub mod constants;
pub mod errors;
pub mod files;
pub mod git;
pub mod messages;
pub mod services;
pub mod tui;
pub mod utils;
pub mod worktree;

use std::process::ExitCode;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Top-level entry point invoked by `main.rs`.
///
/// Returns the desired process exit code.
pub fn run() -> Result<ExitCode, anyhow::Error> {
    cli::run()
}

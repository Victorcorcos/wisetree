//! File-pattern matching and worktree-bootstrapping side-effects.

pub mod patterns;
pub mod service;

pub use patterns::{match_files, normalize_patterns, should_ignore_file};
pub use service::{
    copy_files, execute_post_create_commands, open_terminal, CommandRun, CopyReport, TerminalLaunch,
};

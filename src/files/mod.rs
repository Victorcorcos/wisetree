//! File-pattern matching and worktree-bootstrapping side-effects.

pub mod links;
pub mod patterns;
pub mod service;

pub use links::{link_patterns, unlink_patterns, LinkReport, LinkedEntry};
pub use patterns::{match_files, normalize_patterns, should_ignore_file};
pub use service::{
    copy_files, execute_post_create_commands, open_terminal, open_url, strip_ansi,
    ActivityCallback, ActivityKind, CommandRun, CopyReport, TerminalLaunch,
};

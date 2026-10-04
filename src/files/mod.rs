//! File-pattern matching and worktree-bootstrapping side-effects.

pub mod links;
pub mod patterns;
pub mod service;

pub use links::{
    clear_cache, link_patterns, list_cache, prune_cache, remove_cache_entry, resolve_cache_dir,
    touch_worktree_entry_last_seen, unlink_patterns, unregister_worktree_user, CacheEntryInfo,
    CacheOverview, CachePruneReport, CacheUser, LinkReport, LinkedEntry,
};
pub use patterns::{match_files, normalize_patterns, should_ignore_file};
pub use service::{
    copy_files, execute_post_create_commands, open_terminal, open_url, strip_ansi,
    ActivityCallback, ActivityKind, CommandRun, CopyReport, TerminalLaunch,
};

//! `Screen` enum used by the event loop to dispatch input + render. Mirrors
//! upstream's `AppMode` plus the menu-only `Setup` variant.

use crate::cli::AppMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Menu,
    Create,
    Dashboard,
    Delete,
    MergePullRequest,
    UpdatePullRequest,
    Settings,
    Setup,
}

impl Screen {
    pub fn from_mode(mode: AppMode) -> Self {
        match mode {
            AppMode::Menu => Self::Menu,
            AppMode::Create => Self::Create,
            AppMode::Dashboard => Self::Dashboard,
            AppMode::Settings => Self::Settings,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Menu => "menu",
            Self::Create => "create",
            Self::Dashboard => "dashboard",
            Self::Delete => "delete",
            Self::MergePullRequest => "merge_pull_request",
            Self::UpdatePullRequest => "update_pull_request",
            Self::Settings => "settings",
            Self::Setup => "setup",
        }
    }
}

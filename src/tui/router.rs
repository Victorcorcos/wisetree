//! `Screen` enum used by the event loop to dispatch input + render. Mirrors
//! upstream's `AppMode` plus the menu-only `Setup` variant.

use crate::cli::AppMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Menu,
    Create,
    Dashboard,
    Delete,
    Cache,
    MergePullRequest,
    UpdatePullRequest,
    EnrichPullRequest,
    FixPullRequest,
    ReviewPullRequest,
    BugkillPullRequest,
    UpdateBranch,
    Settings,
    Setup,
    SetupProject,
    AiModelPicker,
}

impl Screen {
    pub fn from_mode(mode: AppMode) -> Self {
        match mode {
            AppMode::Menu => Self::Menu,
            AppMode::Create => Self::Create,
            AppMode::Dashboard => Self::Dashboard,
            AppMode::Cache => Self::Cache,
            AppMode::Settings => Self::Settings,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Menu => "menu",
            Self::Create => "create",
            Self::Dashboard => "dashboard",
            Self::Delete => "delete",
            Self::Cache => "cache",
            Self::MergePullRequest => "merge_pull_request",
            Self::UpdatePullRequest => "update_pull_request",
            Self::EnrichPullRequest => "enrich_pull_request",
            Self::FixPullRequest => "fix_pull_request",
            Self::ReviewPullRequest => "review_pull_request",
            Self::BugkillPullRequest => "bugkill_pull_request",
            Self::UpdateBranch => "update_branch",
            Self::Settings => "settings",
            Self::Setup => "setup",
            Self::SetupProject => "setup_project",
            Self::AiModelPicker => "ai_model_picker",
        }
    }
}

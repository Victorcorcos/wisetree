//! Cross-cutting services (app state, update check, shell integration).

pub mod app_state;
pub mod dashboard;
pub mod presets;
pub mod shell_integration;
pub mod update;

pub use app_state::AppStateService;
pub use dashboard::{
    default_dashboard_warning, is_behind, resolve_base_ref, resolve_dashboard_columns, CheckStatus,
    CommitSummary, DashboardNotice, DashboardNoticeLevel, DashboardRow, DashboardService,
    DashboardUpdate, DashboardWatch, MergeStatus, PrState, PullRequest, PullRequestDetails,
    ReviewStatus, ReviewerSummary, UpdateBranchOutcome, UpdatePhase, UpdateProgress,
    UpdatePullRequestOutcome, BASE_REF_PRIORITY, PR_REFRESH_PERIOD_MS,
};
pub use shell_integration::{
    detect_shell, detect_shell_integration, generate_setup_block, get_config_path,
    install_shell_integration, remove_shell_integration, Shell, ShellIntegrationStatus,
};
pub use update::{
    check_for_updates, check_for_updates_all_sources, get_cached_update_status,
    should_check_for_updates, MultiSourceUpdateResult, UpdateCheckResult, UpdateSource,
};

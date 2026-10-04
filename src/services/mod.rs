//! Dashboard and shell integration services.
pub mod dashboard;
pub mod presets;
pub mod shell_integration;
pub use dashboard::{
    default_dashboard_warning, resolve_base_ref, resolve_dashboard_columns, CheckStatus,
    CommitSummary, DashboardNotice, DashboardNoticeLevel, DashboardRow, DashboardService,
    DashboardUpdate, DashboardWatch, MergeStatus, PrState, PullRequest, PullRequestDetails,
    ReviewStatus, ReviewerSummary, BASE_REF_PRIORITY, PR_REFRESH_PERIOD_MS,
};
pub use shell_integration::{
    detect_shell, detect_shell_integration, generate_setup_block, get_config_path,
    install_shell_integration, remove_shell_integration, Shell, ShellIntegrationStatus,
};

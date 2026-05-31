//! Cross-cutting services (app state, update check, shell integration).

pub mod ai_status;
pub mod app_state;
pub mod dashboard;
pub mod opencode_models;
pub mod presets;
pub mod shell_integration;
pub mod update;

pub use ai_status::{
    canonical_key, AiHarness, AiHarnessState, AiStatus, AiStatusIndex, AiStatusPaths,
    AiStatusReport, AiStatusService,
};
pub use app_state::AppStateService;
pub use dashboard::{
    default_dashboard_warning, is_behind, parse_pull_request_md, resolve_base_ref,
    resolve_dashboard_columns, CheckStatus, CommitSummary, DashboardNotice, DashboardNoticeLevel,
    DashboardRow, DashboardService, DashboardUpdate, DashboardWatch, FillPreparation,
    FillSubmitOutcome, FillSubmitRequest, MergeStatus, PrState, PullRequest, PullRequestDetails,
    ReviewStatus, ReviewerSummary, UpdateBranchOutcome, UpdatePhase, UpdateProgress,
    UpdatePullRequestOutcome, AI_STATUS_BUDGET_MS, BASE_REF_PRIORITY, PR_REFRESH_PERIOD_MS,
};
pub use opencode_models::{fetch_free_opencode_models, fetch_opencode_models, OpencodeModel};
pub use shell_integration::{
    detect_shell, detect_shell_integration, generate_setup_block, get_config_path,
    install_shell_integration, remove_shell_integration, Shell, ShellIntegrationStatus,
};
pub use update::{
    check_for_updates, get_cached_update_status, should_check_for_updates, UpdateCheckResult,
};

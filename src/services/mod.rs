//! Cross-cutting services (app state, update check, shell integration).

pub mod ai_models;
pub mod ai_run;
pub mod ai_status;
pub mod app_state;
pub mod bugkill;
pub mod dashboard;
pub mod develop;
pub mod opencode_models;
pub mod opencode_turn;
pub mod presets;
pub mod review_telemetry;
pub(crate) mod reviewer_evidence;
pub(crate) mod reviewer_routing;
pub(crate) mod reviewer_tests;
pub mod shell_integration;
pub mod update;

pub use ai_models::{
    fetch_claude_effort_levels, fetch_codex_reasoning_levels, parse_claude_effort_levels,
    parse_codex_bundled_models,
};
pub use ai_run::{AiCapturedRun, AiCommand, AiPermission, AiRunMode, AiRunRequest, AiRunner};
pub use ai_status::{
    canonical_key, AiHarness, AiHarnessState, AiStatus, AiStatusIndex, AiStatusPaths,
    AiStatusReport, AiStatusService,
};
pub use app_state::AppStateService;
pub use bugkill::{
    compute_attempt_changes, normalize_hypotheses, parse_hypotheses, transcript_tail,
    AttemptChanges, BugHypothesis, BugkillVerdict, EvidenceQuality, JudgeResult,
    ParsedInvestigation,
};
pub use dashboard::{
    build_review_summary, build_review_summary_with_overview, default_dashboard_warning,
    develop_commit_subject, is_behind, parse_pull_request_md, resolve_base_ref,
    resolve_dashboard_columns, split_duplicate_findings, BugkillPreflight, BugkillPreflightOutcome,
    BugkillResumeState, BugkillSnapshot, BugkillUnverdicted, CheckStatus, CommentGroup,
    CommitSummary, DashboardNotice, DashboardNoticeLevel, DashboardRow, DashboardService,
    DashboardUpdate, DashboardWatch, DevelopCheckOutcome, DevelopPreflight,
    DevelopPreflightOutcome, DevelopResumeState, ExplainPreparation, ExplainSubmitOutcome,
    ExplainSubmitRequest, FixApplyHandoff, FixCommitOutcome, FixPlan, FixPreparation, FixVerdict,
    MergeStatus, PrState, PullRequest, PullRequestDetails, ReviewBenchmarkOutcome, ReviewComment,
    ReviewContext, ReviewFile, ReviewFinding, ReviewPreparation, ReviewScanAttempt, ReviewScanMode,
    ReviewSeverity, ReviewSkippedFile, ReviewStatus, ReviewSummaryAttempt, ReviewVerification,
    ReviewVerificationAttempt, ReviewerSummary, UpdateBranchOutcome, UpdatePhase, UpdateProgress,
    UpdatePullRequestOutcome, AI_STATUS_BUDGET_MS, BASE_REF_PRIORITY, PR_REFRESH_PERIOD_MS,
};
pub use develop::{parse_plan_transcript, summarize_transcript, DevelopPlan, PlanSection};
pub use opencode_models::{
    fetch_free_opencode_models, fetch_opencode_model_variants, fetch_opencode_models, OpencodeModel,
};
pub use opencode_turn::{OpencodeTurn, OpencodeTurnWatcher};
pub use review_telemetry::{
    opencode_usage_for_title, review_scan_title, ReviewScanTelemetry, ReviewTokenUsage,
};
pub use shell_integration::{
    detect_shell, detect_shell_integration, generate_setup_block, get_config_path,
    install_shell_integration, remove_shell_integration, Shell, ShellIntegrationStatus,
};
pub use update::{
    check_for_updates, check_for_updates_all_sources, get_cached_update_status,
    should_check_for_updates, MultiSourceUpdateResult, UpdateCheckResult, UpdateSource,
};

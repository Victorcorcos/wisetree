//! Config + AppState schema definitions.
//!
//! Field names use serde rename to keep wire-format parity with the upstream
//! `.branchlet.json` (camelCase keys). Defaults match the upstream defaults
//! exactly.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub enum LinkStrategy {
    /// Create an empty cache directory and link to it.
    #[default]
    CreateEmpty,
    /// Seed the cache from the source worktree when present.
    SeedFromSource,
    /// Seed only when the source directory already exists.
    SeedIfPresent,
}

/// Default `worktreeCopyPatterns`.
pub fn default_copy_patterns() -> Vec<String> {
    vec![".env*".to_string(), ".vscode/**".to_string()]
}

/// Default `worktreeCopyIgnores`.
pub fn default_copy_ignores() -> Vec<String> {
    vec![
        "**/node_modules/**".to_string(),
        "**/dist/**".to_string(),
        "**/.git/**".to_string(),
        "**/Thumbs.db".to_string(),
        "**/.DS_Store".to_string(),
    ]
}

/// Default `worktreePathTemplate`.
pub fn default_path_template() -> String {
    "$BASE_PATH.worktree".to_string()
}

pub fn default_refresh_ms() -> u64 {
    5_000
}

pub fn default_columns() -> Vec<String> {
    vec![
        "branch".to_string(),
        "status".to_string(),
        "ai_status".to_string(),
        "ahead_behind".to_string(),
        "last_commit".to_string(),
    ]
}

pub fn default_enabled_harnesses() -> Vec<String> {
    vec![
        "claude_code".to_string(),
        "opencode".to_string(),
        "codex_cli".to_string(),
        "gemini_cli".to_string(),
    ]
}

pub fn default_active_window_ms() -> u64 {
    10_000
}

pub fn clamp_interval_ms(value: u64, min_ms: u64, max_ms: u64) -> u64 {
    value.clamp(min_ms, max_ms)
}

pub fn clamp_active_window_ms(value: u64) -> u64 {
    clamp_interval_ms(value, 2_000, 60_000)
}

pub fn clamp_dashboard_refresh_interval(value: u64) -> u64 {
    clamp_interval_ms(value, 5_000, 60_000)
}

pub fn normalize_dashboard_columns(columns: &[String]) -> (Vec<String>, Vec<String>) {
    let mut resolved = Vec::new();
    let mut warnings = Vec::new();

    for column in columns {
        let normalized = column.trim().to_ascii_lowercase();
        let known = matches!(
            normalized.as_str(),
            "branch"
                | "status"
                | "ai_status"
                | "ahead_behind"
                | "diff"
                | "last_commit"
                | "pull_request"
        );

        if !known {
            warnings.push(format!("Unknown dashboard column '{column}' ignored."));
            continue;
        }

        resolved.push(normalized);
    }

    if resolved.is_empty() {
        warnings.push("No valid dashboard columns configured; using defaults.".to_string());
        resolved = default_columns();
    }

    (resolved, warnings)
}

/// AI model + thinking strength used for opencode-assisted flows. Persisted as
/// a nested `ai` object inside the dashboard config:
///
/// ```json
/// "ai": { "model": "opencode/deepseek-v4-flash-free", "thinking": "max" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct AiConfig {
    /// Provider/model selector passed to `opencode run -m <value>` (e.g.
    /// `anthropic/claude-sonnet-4-5`). When empty, AI-assisted conflict
    /// resolution is disabled and the user is asked to resolve manually.
    #[serde(default)]
    pub model: String,

    /// Thinking strength (reasoning effort) paired with `model`, chosen in the
    /// AI model picker — e.g. `low`, `medium`, `high`. Empty means "default"
    /// (no reasoning override). Stored separately from `model` so `model`
    /// stays a clean `provider/model` value for `-m`.
    #[serde(default)]
    pub thinking: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    #[serde(rename = "refreshIntervalMs", default = "default_refresh_ms")]
    pub refresh_interval_ms: u64,

    #[serde(rename = "showPullRequests", default)]
    pub show_pull_requests: bool,

    #[serde(rename = "wiseMerge", default)]
    pub wise_merge: bool,

    #[serde(rename = "columns", default = "default_columns")]
    pub columns: Vec<String>,

    /// AI model + thinking strength for opencode-assisted flows (merge
    /// conflict resolution, PR drafting). When `ai.model` is empty, AI
    /// assistance is disabled and the user resolves conflicts manually.
    #[serde(rename = "ai", default)]
    pub ai: AiConfig,

    #[serde(rename = "aiStatus", default)]
    pub ai_status: AiStatusConfig,

    /// Deprecated location for the notification toggles. Read only for
    /// backward compatibility with configs written before notifications moved
    /// to the top-level [`WorktreeConfig::notifications`] field; folded up by
    /// [`WorktreeConfig::migrate_notifications`] on load and never written back
    /// (`skip_serializing_if` drops it once `None`).
    #[serde(
        rename = "notifications",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub legacy_notifications: Option<NotificationsConfig>,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            refresh_interval_ms: default_refresh_ms(),
            show_pull_requests: false,
            wise_merge: false,
            columns: default_columns(),
            ai: AiConfig::default(),
            ai_status: AiStatusConfig::default(),
            legacy_notifications: None,
        }
    }
}

impl DashboardConfig {
    pub fn clamp(&mut self) {
        self.refresh_interval_ms = clamp_dashboard_refresh_interval(self.refresh_interval_ms);
        self.ai_status.clamp();
    }

    pub fn normalize_columns(&mut self) -> Vec<String> {
        let (mut columns, warnings) = normalize_dashboard_columns(&self.columns);
        if !self.ai_status.enabled_harnesses.is_empty() && !columns.iter().any(|c| c == "ai_status")
        {
            let pos = columns
                .iter()
                .position(|c| c == "status")
                .map(|i| i + 1)
                .unwrap_or(0);
            columns.insert(pos, "ai_status".to_string());
        }
        self.columns = columns;
        warnings
    }
}

/// Opt-in terminal-bell notifications for dashboard-observed events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct NotificationsConfig {
    #[serde(rename = "aiStatusOk", default)]
    pub ai_status_ok: bool,

    #[serde(rename = "prChecksOk", default)]
    pub pr_checks_ok: bool,
}

/// Live `AI Status` column configuration.
///
/// Defaults: every supported harness enabled, 10 s active window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AiStatusConfig {
    /// Per-harness enable list. Supported names:
    /// `claude_code`, `opencode`, `codex_cli`, `gemini_cli`.
    #[serde(rename = "enabledHarnesses", default = "default_enabled_harnesses")]
    pub enabled_harnesses: Vec<String>,

    /// File-write recency threshold for the `Running` state. Clamped to
    /// [2 000, 60 000] ms at load time.
    #[serde(rename = "activeWindowMs", default = "default_active_window_ms")]
    pub active_window_ms: u64,
}

impl Default for AiStatusConfig {
    fn default() -> Self {
        Self {
            enabled_harnesses: default_enabled_harnesses(),
            active_window_ms: default_active_window_ms(),
        }
    }
}

impl AiStatusConfig {
    pub fn clamp(&mut self) {
        self.active_window_ms = clamp_active_window_ms(self.active_window_ms);
    }
}

/// Configuration for the worktree manager.
///
/// Mirrors `WorktreeConfigSchema` from the upstream TS implementation. Field
/// rename keeps JSON shape stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeConfig {
    /// File patterns to copy to new worktrees (glob patterns supported).
    #[serde(rename = "worktreeCopyPatterns", default = "default_copy_patterns")]
    pub worktree_copy_patterns: Vec<String>,

    /// File patterns to ignore when copying (glob patterns supported).
    #[serde(rename = "worktreeCopyIgnores", default = "default_copy_ignores")]
    pub worktree_copy_ignores: Vec<String>,

    /// Template for worktree directory names. Variables: $BASE_PATH,
    /// $WORKTREE_PATH, $BRANCH_NAME, $SOURCE_BRANCH.
    #[serde(rename = "worktreePathTemplate", default = "default_path_template")]
    pub worktree_path_template: String,

    /// Commands to run after creating a worktree. Variables: $BASE_PATH,
    /// $WORKTREE_PATH, $BRANCH_NAME, $SOURCE_BRANCH.
    #[serde(rename = "postCreateCmd", default)]
    pub post_create_cmd: Vec<String>,

    /// Directory patterns to symlink into new worktrees from the shared cache.
    #[serde(rename = "worktreeLinkPatterns", default)]
    pub worktree_link_patterns: Vec<String>,

    /// Strategy used when a link pattern is missing in the source worktree.
    #[serde(rename = "worktreeLinkStrategy", default)]
    pub worktree_link_strategy: LinkStrategy,

    /// Optional override for the shared cache root.
    #[serde(rename = "worktreeLinkCacheDir", default)]
    pub worktree_link_cache_dir: Option<String>,

    /// Command to open terminal in new worktree directory (e.g., 'code $WORKTREE_PATH').
    #[serde(rename = "terminalCommand", default)]
    pub terminal_command: String,

    /// Also delete the associated git branch when deleting a worktree.
    #[serde(rename = "deleteBranchWithWorktree", default)]
    pub delete_branch_with_worktree: bool,

    /// Live dashboard preferences.
    #[serde(rename = "dashboard", default)]
    pub dashboard: DashboardConfig,

    /// Opt-in terminal-bell notifications (AI finished, PR checks passed).
    #[serde(rename = "notifications", default)]
    pub notifications: NotificationsConfig,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            worktree_copy_patterns: default_copy_patterns(),
            worktree_copy_ignores: default_copy_ignores(),
            worktree_path_template: default_path_template(),
            post_create_cmd: Vec::new(),
            worktree_link_patterns: Vec::new(),
            worktree_link_strategy: LinkStrategy::default(),
            worktree_link_cache_dir: None,
            terminal_command: String::new(),
            delete_branch_with_worktree: false,
            dashboard: DashboardConfig::default(),
            notifications: NotificationsConfig::default(),
        }
    }
}

impl WorktreeConfig {
    /// Fold the pre-split `dashboard.notifications` block into the top-level
    /// `notifications` field so configs written before notifications became a
    /// standalone setting keep their bell preferences. The top-level value
    /// wins when both are present; the legacy block is consumed so it is never
    /// written back to disk.
    pub fn migrate_notifications(&mut self) {
        if let Some(legacy) = self.dashboard.legacy_notifications.take() {
            if self.notifications == NotificationsConfig::default() {
                self.notifications = legacy;
            }
        }
    }
}

/// Persistent app state cached at `~/.wisetree/state.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AppState {
    /// Timestamp of last update check (milliseconds since epoch).
    #[serde(rename = "lastUpdateCheck", skip_serializing_if = "Option::is_none")]
    pub last_update_check: Option<u64>,

    /// Latest version available on npm.
    #[serde(rename = "latestVersion", skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,

    /// Version that was current when last checked.
    #[serde(rename = "checkedVersion", skip_serializing_if = "Option::is_none")]
    pub checked_version: Option<String>,
}

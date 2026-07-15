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

/// Model + thinking strength for a single AI-assisted step. The leaf of the
/// per-command [`AiConfig`], e.g.
/// `{ "model": "opencode/deepseek-v4-flash-free", "thinking": "max" }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct AiModelConfig {
    /// Provider/model selector passed to `opencode run -m <value>` (e.g.
    /// `anthropic/claude-sonnet-4-5`). When empty, the AI-assisted step is
    /// disabled and the user is asked to act manually.
    #[serde(default)]
    pub model: String,

    /// Thinking strength (reasoning effort) paired with `model`, chosen in the
    /// AI model picker — e.g. `low`, `medium`, `high`. Empty means "default"
    /// (no reasoning override). Stored separately from `model` so `model`
    /// stays a clean `provider/model` value for `-m`, and passed to opencode
    /// as `--variant <thinking>` where supported.
    #[serde(default)]
    pub thinking: String,
}

// Out-of-the-box defaults per AI command. These point at opencode's free /
// cheap router models so AI-assisted flows work without the user wiring up
// their own keys: `enrich` and `fix.plan` get a roomier reasoning budget
// (`max`), while the heavier-traffic `fix.apply` / `update` run at the model's
// default effort. Used both for `*Config::default()` and as the per-command
// fallback when a slot is absent from the persisted config.
fn default_enrich_ai() -> AiModelConfig {
    AiModelConfig {
        model: "opencode-go/deepseek-v4-flash".to_string(),
        thinking: "max".to_string(),
    }
}
fn default_fix_plan_ai() -> AiModelConfig {
    AiModelConfig {
        model: "opencode-go/glm-5.2".to_string(),
        thinking: "max".to_string(),
    }
}
fn default_fix_apply_ai() -> AiModelConfig {
    AiModelConfig {
        model: "opencode-go/kimi-k2.7-code".to_string(),
        thinking: String::new(),
    }
}
fn default_update_ai() -> AiModelConfig {
    AiModelConfig {
        model: "opencode-go/kimi-k2.7-code".to_string(),
        thinking: String::new(),
    }
}
// Review scans one file's diff per captured call and must reason across
// code smells / security / performance / tests — same strong model as
// `fix.plan`.
fn default_review_ai() -> AiModelConfig {
    default_fix_plan_ai()
}
// Bugkill defaults mirror the Fix pipeline's split: `investigate` reasons
// deeply over the whole codebase (same strong model as `fix.plan`), while
// `fix` edits live and `judge` classifies a short comment (same fast model
// as `fix.apply`).
fn default_bugkill_investigate_ai() -> AiModelConfig {
    default_fix_plan_ai()
}
fn default_bugkill_fix_ai() -> AiModelConfig {
    default_fix_apply_ai()
}
fn default_bugkill_judge_ai() -> AiModelConfig {
    default_fix_apply_ai()
}

/// Per-step models for the two-phase "Fix Pull Request" pipeline. `plan` judges
/// and plans each review comment with a non-interactive `opencode run` (so it
/// can afford a stronger reasoning model); `apply` edits files live in the
/// embedded opencode TUI (and can use a cheaper/faster model).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AiFixConfig {
    #[serde(default = "default_fix_plan_ai")]
    pub plan: AiModelConfig,
    #[serde(default = "default_fix_apply_ai")]
    pub apply: AiModelConfig,
}

impl Default for AiFixConfig {
    fn default() -> Self {
        Self {
            plan: default_fix_plan_ai(),
            apply: default_fix_apply_ai(),
        }
    }
}

/// Per-role models for the "Bugkill" pipeline. `investigate` ranks
/// root-cause hypotheses with a non-interactive `opencode run` (so it can
/// afford a stronger reasoning model); `fix` applies one selected fix live
/// in the embedded opencode TUI; `judge` classifies a freeform "Other"
/// answer as fixed / not fixed with a tiny captured call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AiBugkillConfig {
    #[serde(default = "default_bugkill_investigate_ai")]
    pub investigate: AiModelConfig,
    #[serde(default = "default_bugkill_fix_ai")]
    pub fix: AiModelConfig,
    #[serde(default = "default_bugkill_judge_ai")]
    pub judge: AiModelConfig,
}

impl Default for AiBugkillConfig {
    fn default() -> Self {
        Self {
            investigate: default_bugkill_investigate_ai(),
            fix: default_bugkill_fix_ai(),
            judge: default_bugkill_judge_ai(),
        }
    }
}

/// Per-command AI model + thinking strength for the opencode-assisted flows.
/// Each command (or sub-step) selects its own model so a planning step can use
/// a stronger model than, say, the PR-drafting step. Persisted as a nested
/// `ai` object inside the dashboard config:
///
/// ```json
/// "ai": {
///   "enrich": { "model": "opencode-go/deepseek-v4-flash", "thinking": "max" },
///   "fix": {
///     "plan":  { "model": "opencode-go/glm-5.2", "thinking": "max" },
///     "apply": { "model": "opencode-go/kimi-k2.7-code", "thinking": "default" }
///   },
///   "update": { "model": "opencode-go/kimi-k2.7-code", "thinking": "default" }
/// }
/// ```
///
/// `enrich` drives the "Enrich Pull Request" draft; `fix.plan` / `fix.apply`
/// drive the two Fix phases; `update` drives the AI merge-conflict resolution
/// shared by "Update Pull Request" and "Update branch (locally)".
///
/// An absent slot falls back to its built-in default (free/cheap opencode
/// router models — see `default_*_ai`), so AI flows work out of the box and a
/// fresh AI Settings page is pre-filled. Configs written before per-command
/// models existed used a single flat `{ "model": ..., "thinking": ... }`; those
/// are migrated transparently on load by seeding every command with that one
/// value (which takes precedence over the per-command defaults — see the custom
/// [`Deserialize`] impl below). Serialization always emits the nested shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AiConfig {
    pub enrich: AiModelConfig,
    pub fix: AiFixConfig,
    /// Drives the "Review Pull Request" per-file diff scan.
    pub review: AiModelConfig,
    pub update: AiModelConfig,
    pub bugkill: AiBugkillConfig,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enrich: default_enrich_ai(),
            fix: AiFixConfig::default(),
            review: default_review_ai(),
            update: default_update_ai(),
            bugkill: AiBugkillConfig::default(),
        }
    }
}

impl<'de> Deserialize<'de> for AiConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Accept both the current per-command shape and the legacy flat
        // `{ model, thinking }`. `deny_unknown_fields` keeps the strictness of
        // the prior schema while allowing either set of keys.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            // Legacy flat shape (pre per-command models).
            #[serde(default)]
            model: Option<String>,
            #[serde(default)]
            thinking: Option<String>,
            // Current per-command shape.
            #[serde(default)]
            enrich: Option<AiModelConfig>,
            #[serde(default)]
            fix: Option<AiFixConfig>,
            #[serde(default)]
            review: Option<AiModelConfig>,
            #[serde(default)]
            update: Option<AiModelConfig>,
            #[serde(default)]
            bugkill: Option<AiBugkillConfig>,
        }

        let raw = Raw::deserialize(deserializer)?;

        // A legacy flat value seeds every command and takes precedence over the
        // per-command defaults. An absent slot with no legacy value falls back
        // to that command's built-in default.
        let legacy = if raw.model.is_some() || raw.thinking.is_some() {
            Some(AiModelConfig {
                model: raw.model.unwrap_or_default(),
                thinking: raw.thinking.unwrap_or_default(),
            })
        } else {
            None
        };

        Ok(AiConfig {
            enrich: raw
                .enrich
                .or_else(|| legacy.clone())
                .unwrap_or_else(default_enrich_ai),
            fix: raw.fix.unwrap_or_else(|| match &legacy {
                Some(l) => AiFixConfig {
                    plan: l.clone(),
                    apply: l.clone(),
                },
                None => AiFixConfig::default(),
            }),
            review: raw
                .review
                .or_else(|| legacy.clone())
                .unwrap_or_else(default_review_ai),
            update: raw
                .update
                .or_else(|| legacy.clone())
                .unwrap_or_else(default_update_ai),
            bugkill: raw.bugkill.unwrap_or_else(|| match &legacy {
                Some(l) => AiBugkillConfig {
                    investigate: l.clone(),
                    fix: l.clone(),
                    judge: l.clone(),
                },
                None => AiBugkillConfig::default(),
            }),
        })
    }
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

    /// Per-command AI model + thinking strength for opencode-assisted flows
    /// (enrich, fix plan/apply, update conflict resolution). When a command's
    /// `model` is empty, that AI step is disabled and the user acts manually.
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

#[cfg(test)]
mod ai_config_tests {
    use super::{AiConfig, DashboardConfig};

    #[test]
    fn legacy_flat_ai_seeds_every_command() {
        // Configs written before per-command models used a single flat
        // `{ model, thinking }`. It must migrate by seeding every command.
        let json = r#"{ "model": "opencode/deepseek-v4-flash-free", "thinking": "max" }"#;
        let ai: AiConfig = serde_json::from_str(json).expect("legacy ai parses");
        for leaf in [
            &ai.enrich,
            &ai.fix.plan,
            &ai.fix.apply,
            &ai.review,
            &ai.update,
            &ai.bugkill.investigate,
            &ai.bugkill.fix,
            &ai.bugkill.judge,
        ] {
            assert_eq!(leaf.model, "opencode/deepseek-v4-flash-free");
            assert_eq!(leaf.thinking, "max");
        }
    }

    #[test]
    fn nested_per_command_ai_parses_each_slot() {
        let json = r#"{
            "enrich": { "model": "opencode/deepseek-v4-flash-free", "thinking": "max" },
            "fix": {
                "plan":  { "model": "openai/gpt-5.5", "thinking": "high" },
                "apply": { "model": "opencode-go/kimi-k2.7-code" }
            },
            "update": { "model": "opencode-go/kimi-k2.7-code" }
        }"#;
        let ai: AiConfig = serde_json::from_str(json).expect("nested ai parses");
        assert_eq!(ai.enrich.model, "opencode/deepseek-v4-flash-free");
        assert_eq!(ai.enrich.thinking, "max");
        assert_eq!(ai.fix.plan.model, "openai/gpt-5.5");
        assert_eq!(ai.fix.plan.thinking, "high");
        assert_eq!(ai.fix.apply.model, "opencode-go/kimi-k2.7-code");
        assert_eq!(ai.fix.apply.thinking, ""); // omitted → Default
        assert_eq!(ai.update.model, "opencode-go/kimi-k2.7-code");
    }

    #[test]
    fn empty_ai_object_uses_per_command_defaults() {
        let ai: AiConfig = serde_json::from_str("{}").expect("empty ai parses");
        assert_eq!(ai, AiConfig::default());
        assert_eq!(ai.enrich.model, "opencode-go/deepseek-v4-flash");
        assert_eq!(ai.enrich.thinking, "max");
        assert_eq!(ai.fix.plan.model, "opencode-go/glm-5.2");
        assert_eq!(ai.fix.plan.thinking, "max");
        assert_eq!(ai.fix.apply.model, "opencode-go/kimi-k2.7-code");
        assert_eq!(ai.fix.apply.thinking, "");
        assert_eq!(ai.update.model, "opencode-go/kimi-k2.7-code");
        assert_eq!(ai.update.thinking, "");
        // Bugkill defaults mirror the Fix pipeline's plan/apply split.
        assert_eq!(ai.bugkill.investigate.model, "opencode-go/glm-5.2");
        assert_eq!(ai.bugkill.investigate.thinking, "max");
        assert_eq!(ai.bugkill.fix.model, "opencode-go/kimi-k2.7-code");
        assert_eq!(ai.bugkill.fix.thinking, "");
        assert_eq!(ai.bugkill.judge.model, "opencode-go/kimi-k2.7-code");
        assert_eq!(ai.bugkill.judge.thinking, "");
    }

    #[test]
    fn absent_slot_falls_back_to_its_default_other_slots_explicit() {
        // Only `enrich` configured → fix/update fall back to their defaults.
        let json = r#"{ "enrich": { "model": "x", "thinking": "low" } }"#;
        let ai: AiConfig = serde_json::from_str(json).unwrap();
        assert_eq!(ai.enrich.model, "x");
        assert_eq!(ai.fix.plan.model, "opencode-go/glm-5.2");
        assert_eq!(ai.update.model, "opencode-go/kimi-k2.7-code");
    }

    #[test]
    fn legacy_flat_overrides_per_command_defaults() {
        // A legacy value must win over the per-command defaults (it was an
        // explicit choice), seeding all four slots.
        let json = r#"{ "model": "legacy/model", "thinking": "low" }"#;
        let ai: AiConfig = serde_json::from_str(json).unwrap();
        for leaf in [
            &ai.enrich,
            &ai.fix.plan,
            &ai.fix.apply,
            &ai.review,
            &ai.update,
            &ai.bugkill.investigate,
            &ai.bugkill.fix,
            &ai.bugkill.judge,
        ] {
            assert_eq!(leaf.model, "legacy/model");
            assert_eq!(leaf.thinking, "low");
        }
    }

    #[test]
    fn unknown_ai_key_is_rejected() {
        // Strictness is preserved: a typo'd key must error, not silently drop.
        let json = r#"{ "enrich": { "model": "x" }, "bogus": {} }"#;
        assert!(serde_json::from_str::<AiConfig>(json).is_err());
    }

    #[test]
    fn unknown_bugkill_key_is_rejected() {
        let json = r#"{ "bugkill": { "investigate": { "model": "x" }, "bogus": {} } }"#;
        assert!(serde_json::from_str::<AiConfig>(json).is_err());
    }

    #[test]
    fn partial_bugkill_slot_falls_back_per_slot() {
        // Only `bugkill.judge` configured → investigate/fix fall back to their
        // own defaults, never to judge's value.
        let json = r#"{ "bugkill": { "judge": { "model": "tiny/judge", "thinking": "low" } } }"#;
        let ai: AiConfig = serde_json::from_str(json).unwrap();
        assert_eq!(ai.bugkill.judge.model, "tiny/judge");
        assert_eq!(ai.bugkill.judge.thinking, "low");
        assert_eq!(ai.bugkill.investigate.model, "opencode-go/glm-5.2");
        assert_eq!(ai.bugkill.investigate.thinking, "max");
        assert_eq!(ai.bugkill.fix.model, "opencode-go/kimi-k2.7-code");
        assert_eq!(ai.bugkill.fix.thinking, "");
    }

    #[test]
    fn round_trips_through_nested_shape() {
        // A legacy config, once loaded, serializes back as the nested shape.
        let legacy = r#"{ "model": "m", "thinking": "high" }"#;
        let ai: AiConfig = serde_json::from_str(legacy).unwrap();
        let serialized = serde_json::to_string(&ai).unwrap();
        assert!(serialized.contains("\"enrich\""));
        assert!(serialized.contains("\"fix\""));
        assert!(serialized.contains("\"update\""));
        assert!(serialized.contains("\"bugkill\""));
        let reparsed: AiConfig = serde_json::from_str(&serialized).unwrap();
        assert_eq!(ai, reparsed);
    }

    #[test]
    fn dashboard_config_default_ai_uses_per_command_defaults() {
        let dash = DashboardConfig::default();
        assert_eq!(dash.ai, AiConfig::default());
        assert_eq!(dash.ai.fix.plan.model, "opencode-go/glm-5.2");
    }
}

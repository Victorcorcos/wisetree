//! Config + AppState schema definitions.
//!
//! Field names use serde rename to keep wire-format parity with the upstream
//! `.branchlet.json` (camelCase keys). Defaults match the upstream defaults
//! exactly.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::constants::DEFAULT_AI_MODEL_ID;
use crate::services::opencode_models::{free_models, OpencodeModel};

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
        "ahead_behind".to_string(),
        "last_commit".to_string(),
    ]
}

pub fn clamp_dashboard_refresh_interval(value: u64) -> u64 {
    value.clamp(5_000, 60_000)
}

pub fn normalize_dashboard_columns(columns: &[String]) -> (Vec<String>, Vec<String>) {
    let mut resolved = Vec::new();
    let mut warnings = Vec::new();

    for column in columns {
        let normalized = column.trim().to_ascii_lowercase();
        let known = matches!(
            normalized.as_str(),
            "branch" | "status" | "ahead_behind" | "last_commit" | "pull_request"
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    #[serde(rename = "refreshIntervalMs", default = "default_refresh_ms")]
    pub refresh_interval_ms: u64,

    #[serde(rename = "showPullRequests", default)]
    pub show_pull_requests: bool,

    #[serde(rename = "columns", default = "default_columns")]
    pub columns: Vec<String>,

    /// AI backend used to resolve merge conflicts when the
    /// "Update Pull Request" flow detects them. Defaults to the first
    /// free model exposed by the opencode CLI; the list is discovered at
    /// runtime via `crate::services::opencode_models::free_models()`.
    #[serde(rename = "useAi", default)]
    pub use_ai: UseAiConfig,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            refresh_interval_ms: default_refresh_ms(),
            show_pull_requests: false,
            columns: default_columns(),
            use_ai: UseAiConfig::default(),
        }
    }
}

impl DashboardConfig {
    pub fn clamp(&mut self) {
        self.refresh_interval_ms = clamp_dashboard_refresh_interval(self.refresh_interval_ms);
        self.use_ai.clamp();
    }

    pub fn normalize_columns(&mut self) -> Vec<String> {
        let (columns, warnings) = normalize_dashboard_columns(&self.columns);
        self.columns = columns;
        warnings
    }
}

pub fn default_use_ai_model() -> String {
    DEFAULT_AI_MODEL_ID.to_string()
}

/// AI backend selection for the merge-conflict resolver. The shape is a
/// nested object so we can grow it (api keys, fallback models, …) without
/// breaking the existing JSON schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UseAiConfig {
    /// Opencode model id (e.g. `opencode/big-pickle`). Must match an
    /// entry surfaced by the opencode `models` listing for the
    /// `opencode` provider; unknown ids are reset to the first available
    /// free model during `clamp`.
    #[serde(rename = "model", default = "default_use_ai_model")]
    pub model: String,
}

impl Default for UseAiConfig {
    fn default() -> Self {
        Self {
            model: default_use_ai_model(),
        }
    }
}

impl UseAiConfig {
    /// Live list of free opencode models available on this machine. Used
    /// by the settings cycle, the JSON clamp, and the label lookup so a
    /// new free model exposed by `opencode models` shows up in the UI
    /// without a wisetree release.
    pub fn available_models() -> &'static [OpencodeModel] {
        free_models()
    }

    /// Snap unknown ids back to the first available free model so a
    /// hand-edited config (or a model that opencode silently retired)
    /// can't land the merge pipeline on a non-existent backend.
    pub fn clamp(&mut self) {
        let models = Self::available_models();
        if models.iter().any(|m| m.id == self.model) {
            return;
        }
        self.model = models
            .first()
            .map(|m| m.id.clone())
            .unwrap_or_else(default_use_ai_model);
    }

    /// Human-readable label for the currently selected model. Falls back
    /// to the raw id when the model is unknown so the UI never blanks.
    pub fn label(&self) -> &str {
        Self::available_models()
            .iter()
            .find(|m| m.id == self.model)
            .map(|m| m.label.as_str())
            .unwrap_or(self.model.as_str())
    }

    /// Index of the current model in `available_models()`, or 0 when the
    /// model id isn't recognised — keeps "cycle to next" arithmetic
    /// total even if a hand-edited config drifts.
    pub fn index(&self) -> usize {
        Self::available_models()
            .iter()
            .position(|m| m.id == self.model)
            .unwrap_or(0)
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
        }
    }
}

#[cfg(test)]
mod use_ai_tests {
    use super::*;

    #[test]
    fn default_model_is_in_free_list() {
        let cfg = UseAiConfig::default();
        let known: Vec<&str> = UseAiConfig::available_models()
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert!(
            known.contains(&cfg.model.as_str()),
            "default model `{}` should be in the free model list: {:?}",
            cfg.model,
            known,
        );
        // `label()` must produce a non-empty string for any default config
        // so the settings rect never renders blank.
        assert!(!cfg.label().is_empty());
    }

    #[test]
    fn clamp_resets_unknown_model_to_first_available_free() {
        let mut cfg = UseAiConfig {
            model: "made-up/model".to_string(),
        };
        cfg.clamp();
        let expected = UseAiConfig::available_models()
            .first()
            .map(|m| m.id.clone())
            .unwrap_or_else(|| DEFAULT_AI_MODEL_ID.to_string());
        assert_eq!(cfg.model, expected);
    }

    #[test]
    fn clamp_preserves_known_model() {
        if let Some(model) = UseAiConfig::available_models().last() {
            let mut cfg = UseAiConfig {
                model: model.id.clone(),
            };
            cfg.clamp();
            assert_eq!(cfg.model, model.id);
        }
    }

    #[test]
    fn dashboard_config_round_trips_use_ai() {
        let mut cfg = DashboardConfig::default();
        cfg.use_ai.model = DEFAULT_AI_MODEL_ID.to_string();
        let serialized = serde_json::to_string(&cfg).unwrap();
        let parsed: DashboardConfig = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed.use_ai.model, DEFAULT_AI_MODEL_ID);
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

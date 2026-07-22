//! Compile-time and runtime constants — config paths, app metadata.

use std::path::PathBuf;

/// Filename of the project-local config (lives next to the repo root).
pub const LOCAL_CONFIG_FILE_NAME: &str = ".wisetree.json";

/// Subdirectory of `$HOME` where the global config and state live.
pub const GLOBAL_CONFIG_DIR_NAME: &str = ".wisetree";

/// Filename of the global config.
pub const GLOBAL_CONFIG_FILE_NAME: &str = "settings.json";

/// Filename of the app state cache.
pub const APP_STATE_FILE_NAME: &str = "state.json";

/// Subdirectory under `~/.wisetree/` that stores per-repository shared caches.
pub const CACHE_DIR_NAME: &str = "cache";

/// Filename of the dashboard pull-request cache.
pub const DASHBOARD_PR_CACHE_FILE_NAME: &str = "dashboard_pr_cache.json";

/// Filename of the bounded Review Pull Request scan-telemetry history.
pub const REVIEW_TELEMETRY_FILE_NAME: &str = "review_telemetry.json";

/// Commit message title written when the "Update Pull Request" flow
/// committed the result of an AI-assisted conflict resolution. Kept as a
/// constant so downstream tooling (release notes, blame heuristics) can
/// recognise the synthetic commit.
pub const UPDATE_MERGE_COMMIT_MESSAGE: &str = "Merging and solving conflicts";

/// CLI binary name used for AI-assisted merge conflict resolution.
pub const OPENCODE_CLI_BINARY: &str = "opencode";

/// Subdirectory under the XDG state home where opencode keeps its state.
pub const OPENCODE_STATE_DIR_NAME: &str = "opencode";

/// Filename of opencode's persisted per-model state (recent/favorite/variant).
pub const OPENCODE_MODEL_STATE_FILE_NAME: &str = "model.json";

/// Resolve the global config directory (`~/.wisetree/`).
///
/// Mirrors the upstream behaviour of synthesising the path from `$HOME`. We
/// fall back to `dirs::home_dir` when `$HOME` isn't set.
pub fn global_config_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(GLOBAL_CONFIG_DIR_NAME);
        }
    }

    dirs::home_dir()
        .map(|h| h.join(GLOBAL_CONFIG_DIR_NAME))
        .unwrap_or_else(|| PathBuf::from(GLOBAL_CONFIG_DIR_NAME))
}

/// Path to the global config file (`~/.wisetree/settings.json`).
pub fn global_config_file() -> PathBuf {
    global_config_dir().join(GLOBAL_CONFIG_FILE_NAME)
}

/// Path to the app state cache (`~/.wisetree/state.json`).
pub fn app_state_file() -> PathBuf {
    global_config_dir().join(APP_STATE_FILE_NAME)
}

/// Path to the cache root (`~/.wisetree/cache/`).
pub fn global_cache_dir() -> PathBuf {
    global_config_dir().join(CACHE_DIR_NAME)
}

/// Path to the dashboard PR cache (`~/.wisetree/dashboard_pr_cache.json`).
pub fn dashboard_pr_cache_file() -> PathBuf {
    global_config_dir().join(DASHBOARD_PR_CACHE_FILE_NAME)
}

/// Path to the bounded Review Pull Request telemetry history.
pub fn review_telemetry_file() -> PathBuf {
    global_config_dir().join(REVIEW_TELEMETRY_FILE_NAME)
}

/// Resolve opencode's persisted model-state file
/// (`$XDG_STATE_HOME/opencode/model.json`, defaulting to
/// `~/.local/state/opencode/model.json`).
///
/// opencode derives this path through the `xdg-basedir` package: it honours
/// `$XDG_STATE_HOME` when set and non-empty, otherwise `$HOME/.local/state`. We
/// mirror that resolution exactly so the file we seed is the same one the
/// opencode TUI reads on launch to pick a model's reasoning-effort variant.
pub fn opencode_model_state_file() -> PathBuf {
    if let Ok(state_home) = std::env::var("XDG_STATE_HOME") {
        if !state_home.is_empty() {
            return PathBuf::from(state_home)
                .join(OPENCODE_STATE_DIR_NAME)
                .join(OPENCODE_MODEL_STATE_FILE_NAME);
        }
    }

    let home = std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_default();
    home.join(".local")
        .join("state")
        .join(OPENCODE_STATE_DIR_NAME)
        .join(OPENCODE_MODEL_STATE_FILE_NAME)
}

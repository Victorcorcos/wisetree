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

/// Filename of the dashboard pull-request cache.
pub const DASHBOARD_PR_CACHE_FILE_NAME: &str = "dashboard_pr_cache.json";

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

/// Path to the dashboard PR cache (`~/.wisetree/dashboard_pr_cache.json`).
pub fn dashboard_pr_cache_file() -> PathBuf {
    global_config_dir().join(DASHBOARD_PR_CACHE_FILE_NAME)
}

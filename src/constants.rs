//! Compile-time and runtime constants — config paths, app metadata.

use std::path::{Path, PathBuf};

/// Directory under a repository root holding every repository-local Wisetree
/// artifact: the project config, guides, Split state and Review history.
pub const LOCAL_DIR_NAME: &str = ".wisetree";

/// Filename of the project-local config, inside [`LOCAL_DIR_NAME`].
pub const LOCAL_CONFIG_FILE_NAME: &str = ".wisetree.json";

/// Subdirectory of `$HOME` where the global config and state live.
pub const GLOBAL_CONFIG_DIR_NAME: &str = ".wisetree";

/// Filename of the global config.
pub const GLOBAL_CONFIG_FILE_NAME: &str = "settings.json";

/// Filename of the app state cache.
pub const APP_STATE_FILE_NAME: &str = "state.json";

/// Subdirectory under `~/.wisetree/` that stores per-repository shared caches.
pub const CACHE_DIR_NAME: &str = "cache";

/// Subdirectory under the global cache used for images attached to prompts.
/// It deliberately lives outside worktrees so uploads never become untracked
/// project files.
pub const IMAGE_UPLOADS_DIR_NAME: &str = "uploads";

/// Attachments not referenced by a live or resumable workflow become eligible
/// for cleanup after this long. This leaves abandoned drafts recoverable while
/// keeping the global content-addressed store bounded over time.
pub const IMAGE_UPLOAD_RETENTION_DAYS: u64 = 30;

/// A single cleanup pass never deletes more than this many files.
pub const IMAGE_UPLOAD_CLEANUP_LIMIT: usize = 64;

/// Largest image accepted, matching Claude's documented 10 MB per-image API
/// limit. Rejecting here turns a mid-run harness failure into an immediate,
/// actionable message in the textarea.
pub const IMAGE_UPLOAD_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Filename of the dashboard pull-request cache.
pub const DASHBOARD_PR_CACHE_FILE_NAME: &str = "dashboard_pr_cache.json";

/// Directory holding every repository-local Review artifact, relative to a
/// repository root. Review history is about one repository's pull requests,
/// so it is stored with that repository rather than in the global config.
pub const REVIEW_DIR_NAME: &str = ".wisetree/review";

/// Filename of the bounded Review Pull Request run history: every summary row
/// of the last few runs plus their per-call token telemetry, so a run too long
/// for the table stays diagnosable after the screen closes.
pub const REVIEW_REPORT_FILE_NAME: &str = "report.json";

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

/// Path to Wisetree-owned prompt image storage (`~/.wisetree/cache/uploads/`).
pub fn image_uploads_dir() -> PathBuf {
    global_cache_dir().join(IMAGE_UPLOADS_DIR_NAME)
}

/// Path to the dashboard PR cache (`~/.wisetree/dashboard_pr_cache.json`).
pub fn dashboard_pr_cache_file() -> PathBuf {
    global_config_dir().join(DASHBOARD_PR_CACHE_FILE_NAME)
}

/// Path to a repository's project config: `<root>/.wisetree/.wisetree.json`.
/// This is where Wisetree writes, and the first place it reads.
pub fn local_config_file(repo_root: &Path) -> PathBuf {
    repo_root.join(LOCAL_DIR_NAME).join(LOCAL_CONFIG_FILE_NAME)
}

/// The pre-move location: `.wisetree.json` loose at the repository root.
/// Still read when the current location is absent, so a repository that was
/// never migrated keeps working instead of silently falling back to global
/// defaults. Nothing writes here.
pub fn legacy_local_config_file(repo_root: &Path) -> PathBuf {
    repo_root.join(LOCAL_CONFIG_FILE_NAME)
}

/// The repository root owning a project-config path, which is what config
/// discovery takes. Handles both `<root>/.wisetree/.wisetree.json` and the
/// legacy `<root>/.wisetree.json`, so callers never have to know which one
/// they are holding — `config_path.parent()` would be wrong for the former.
pub fn local_config_root(config_path: &Path) -> Option<&Path> {
    let parent = config_path.parent()?;
    if parent.file_name() == Some(std::ffi::OsStr::new(LOCAL_DIR_NAME)) {
        parent.parent()
    } else {
        Some(parent)
    }
}

/// The project config Wisetree should read for `repo_root`, if any: the
/// current location when it exists, otherwise the legacy root file.
pub fn existing_local_config_file(repo_root: &Path) -> Option<PathBuf> {
    [
        local_config_file(repo_root),
        legacy_local_config_file(repo_root),
    ]
    .into_iter()
    .find(|path| path.exists())
}

/// Path to a repository-local Review artifact.
///
/// Review history describes one repository's pull requests, so it belongs to
/// that repository. It is anchored on the *mother* worktree rather than the
/// worktree the run happened in: Wisetree exists to create and delete
/// worktrees, and history written into a throwaway checkout dies with it.
pub fn review_artifact_file(worktree_path: &Path, file_name: &str) -> PathBuf {
    crate::services::guides::mother_worktree(worktree_path)
        .unwrap_or_else(|| worktree_path.to_path_buf())
        .join(REVIEW_DIR_NAME)
        .join(file_name)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_config_lives_inside_the_wisetree_directory() {
        let root = Path::new("/repo");

        assert_eq!(
            local_config_file(root),
            Path::new("/repo/.wisetree/.wisetree.json")
        );
        assert_eq!(
            legacy_local_config_file(root),
            Path::new("/repo/.wisetree.json")
        );
    }

    /// A repository that was never migrated keeps working: without the
    /// fallback its config would be ignored in silence and Wisetree would
    /// quietly run on global defaults.
    #[test]
    fn a_legacy_root_config_is_read_when_the_current_one_is_absent() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(legacy_local_config_file(repo.path()), "{}").unwrap();

        assert_eq!(
            existing_local_config_file(repo.path()),
            Some(legacy_local_config_file(repo.path()))
        );
    }

    #[test]
    fn the_current_location_wins_when_both_exist() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(legacy_local_config_file(repo.path()), "{}").unwrap();
        let current = local_config_file(repo.path());
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(&current, "{}").unwrap();

        assert_eq!(existing_local_config_file(repo.path()), Some(current));
    }

    #[test]
    fn a_repository_without_any_project_config_resolves_to_nothing() {
        let repo = tempfile::tempdir().unwrap();

        assert_eq!(existing_local_config_file(repo.path()), None);
    }

    /// Config discovery takes the repository root, not the directory holding
    /// the file — which stopped being the same thing once the config moved
    /// one level down.
    #[test]
    fn the_owning_root_is_recovered_from_either_layout() {
        assert_eq!(
            local_config_root(Path::new("/repo/.wisetree/.wisetree.json")),
            Some(Path::new("/repo"))
        );
        assert_eq!(
            local_config_root(Path::new("/repo/.wisetree.json")),
            Some(Path::new("/repo"))
        );
    }
}

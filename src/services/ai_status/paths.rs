//! Cross-platform path resolution for AI harness state directories.
//!
//! Three of the four supported harnesses (Claude Code, codex-cli, gemini-cli)
//! put their state under `~/.<name>/…` on every OS — `dirs::home_dir()` is the
//! correct primitive there. Opencode is special: it uses an XDG-style layout
//! everywhere (including macOS and Windows), so we replicate the
//! `xdg-basedir` resolution explicitly. See `PLAN.md` §3.1 for the rationale.

use std::path::{Path, PathBuf};

/// Locations of the on-disk state directories each harness writes to.
///
/// Constructed via [`AiStatusPaths::detect`] in production and assembled
/// manually in tests so detection can run against a hermetic `tempfile::TempDir`
/// without touching the developer's real `$HOME`.
#[derive(Debug, Clone, Default)]
pub struct AiStatusPaths {
    pub claude_projects: Option<PathBuf>,
    pub claude_sessions: Option<PathBuf>,
    pub codex_sessions: Option<PathBuf>,
    pub gemini_tmp: Option<PathBuf>,
    pub opencode_state: Option<PathBuf>,
    pub opencode_data: Option<PathBuf>,
}

impl AiStatusPaths {
    /// Resolve every harness path from the current process environment.
    pub fn detect() -> Self {
        Self {
            claude_projects: claude_projects_dir(),
            claude_sessions: claude_sessions_dir(),
            codex_sessions: codex_sessions_dir(),
            gemini_tmp: gemini_tmp_dir(),
            opencode_state: opencode_state_dir(),
            opencode_data: opencode_data_dir(),
        }
    }
}

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

fn claude_projects_dir() -> Option<PathBuf> {
    claude_root().map(|p| p.join("projects"))
}

fn claude_sessions_dir() -> Option<PathBuf> {
    claude_root().map(|p| p.join("sessions"))
}

fn claude_root() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".claude")))
}

fn codex_sessions_dir() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")))
        .map(|p| p.join("sessions"))
}

fn gemini_tmp_dir() -> Option<PathBuf> {
    std::env::var_os("GEMINI_CLI_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".gemini")))
        .map(|p| p.join("tmp"))
}

// Replicates the `xdg-basedir` resolution that opencode itself uses on every
// OS (see PLAN.md §3.1). Do NOT substitute `dirs::state_dir()` /
// `dirs::data_local_dir()` — those return platform-native paths and would
// miss every opencode install on macOS and Windows.
fn xdg_dir(env_var: &str, suffix: &str) -> Option<PathBuf> {
    std::env::var_os(env_var)
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(suffix)))
}

fn opencode_state_dir() -> Option<PathBuf> {
    xdg_dir("XDG_STATE_HOME", ".local/state").map(|p| p.join("opencode"))
}

fn opencode_data_dir() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", ".local/share").map(|p| p.join("opencode"))
}

/// Canonicalize a worktree path into the form used as the map key for
/// cross-side comparisons. See `PLAN.md` §3.3 for the mismatch sources.
pub fn canonical_key(path: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    normalized_key(&resolved)
}

fn normalized_key(path: &Path) -> PathBuf {
    let stripped = dunce::simplified(path).to_path_buf();
    let normalized: PathBuf = stripped.components().collect();
    #[cfg(target_os = "macos")]
    {
        // HFS+/APFS are case-insensitive by default; lowercase the string form
        // so paths recorded as `/Users/Foo` match `/Users/foo`.
        PathBuf::from(normalized.to_string_lossy().to_lowercase())
    }
    #[cfg(not(target_os = "macos"))]
    {
        normalized
    }
}

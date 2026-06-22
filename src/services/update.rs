//! `UpdateService` — best-effort npm-registry version check with a
//! 24-hour cache backed by `AppStateService`. Mirrors upstream's
//! `update-service.ts` (`shouldCheckForUpdates`, `checkForUpdates`,
//! `getCachedUpdateStatus`).
//!
//! `check_for_updates_all_sources` extends the same shape to the
//! Homebrew tap: it fetches both the npm registry and the formula at
//! `victorcorcos/homebrew-tap` in parallel and returns the per-source
//! results so the "Check for Updates" screen can render two rectangles.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::services::app_state::AppStateService;
use crate::utils::is_newer_version;

const NPM_REGISTRY_URL: &str = "https://registry.npmjs.org/wisetree/latest";
const HOMEBREW_FORMULA_URL: &str =
    "https://raw.githubusercontent.com/victorcorcos/homebrew-tap/main/Formula/wisetree.rb";
const CACHE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateSource {
    Npm,
    Homebrew,
}

impl UpdateSource {
    pub fn label(self) -> &'static str {
        match self {
            UpdateSource::Npm => "npm",
            UpdateSource::Homebrew => "homebrew",
        }
    }

    /// Command + argv used to upgrade Wisetree via this source.
    pub fn upgrade_argv(self) -> &'static [&'static str] {
        match self {
            UpdateSource::Npm => &["npm", "install", "-g", "wisetree"],
            UpdateSource::Homebrew => &["brew", "upgrade", "victorcorcos/tap/wisetree"],
        }
    }

    /// Human-readable rendering of the upgrade command.
    pub fn upgrade_command_display(self) -> &'static str {
        match self {
            UpdateSource::Npm => "npm install -g wisetree",
            UpdateSource::Homebrew => "brew upgrade victorcorcos/tap/wisetree",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheckResult {
    pub has_update: bool,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub checked_at: u64,
    pub error: Option<String>,
}

/// Combined result for the per-source "Check for Updates" screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiSourceUpdateResult {
    pub current_version: String,
    pub npm: UpdateCheckResult,
    pub homebrew: UpdateCheckResult,
}

impl MultiSourceUpdateResult {
    pub fn source(&self, source: UpdateSource) -> &UpdateCheckResult {
        match source {
            UpdateSource::Npm => &self.npm,
            UpdateSource::Homebrew => &self.homebrew,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn should_check_for_updates(svc: &AppStateService) -> bool {
    let last = svc.state().last_update_check.unwrap_or(0);
    now_ms().saturating_sub(last) >= CACHE_TTL_MS
}

/// Returns the cached update status if one is on file, else `None`.
pub fn get_cached_update_status(
    svc: &AppStateService,
    current_version: Option<&str>,
) -> Option<UpdateCheckResult> {
    let state = svc.state();
    let last = state.last_update_check?;
    let latest = state.latest_version.as_deref()?;
    let version = current_version
        .or(state.checked_version.as_deref())
        .unwrap_or(latest);
    let has_update = is_newer_version(version, latest);
    Some(UpdateCheckResult {
        has_update,
        current_version: version.to_string(),
        latest_version: Some(latest.to_string()),
        checked_at: last,
        error: None,
    })
}

/// Runs the registry check. Honors the cache unless `force` is true. Errors
/// are returned as `UpdateCheckResult { error: Some(...) }` rather than
/// propagating, mirroring upstream behavior.
pub async fn check_for_updates(
    current_version: &str,
    svc: &mut AppStateService,
    force: bool,
) -> UpdateCheckResult {
    let now = now_ms();
    if !force && !should_check_for_updates(svc) {
        if let Some(cached) = get_cached_update_status(svc, Some(current_version)) {
            return cached;
        }
        return UpdateCheckResult {
            has_update: false,
            current_version: current_version.to_string(),
            latest_version: None,
            checked_at: svc.state().last_update_check.unwrap_or(now),
            error: None,
        };
    }

    match fetch_latest_version().await {
        Ok(latest) => {
            let has_update = is_newer_version(current_version, &latest);
            svc.update(|s| {
                s.last_update_check = Some(now);
                s.latest_version = Some(latest.clone());
                s.checked_version = Some(current_version.to_string());
            });
            svc.save();
            UpdateCheckResult {
                has_update,
                current_version: current_version.to_string(),
                latest_version: Some(latest),
                checked_at: now,
                error: None,
            }
        }
        Err(err) => UpdateCheckResult {
            has_update: false,
            current_version: current_version.to_string(),
            latest_version: None,
            checked_at: now,
            error: Some(err),
        },
    }
}

async fn fetch_latest_version() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(NPM_REGISTRY_URL)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    body.get("version")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "missing version field".to_string())
}

/// Fetch the Homebrew tap formula and pull `version "X.Y.Z"` out of it.
/// The tap is the source of truth for the `brew install victorcorcos/tap/wisetree`
/// channel, so its version is what the `homebrew` rectangle should reflect.
async fn fetch_homebrew_latest_version() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(HOMEBREW_FORMULA_URL)
        .header("Accept", "text/plain")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    parse_formula_version(&body)
}

/// Parse the first `version "X.Y.Z"` line in a Homebrew formula. Exposed
/// (crate-private) so unit tests can pin the parser to known inputs.
pub(crate) fn parse_formula_version(body: &str) -> Result<String, String> {
    for line in body.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("version") else {
            continue;
        };
        // The next non-whitespace char must be a `"` — guards against e.g.
        // `versioned_formula` or other keywords that happen to start with
        // `version`.
        let rest = rest.trim_start();
        if !rest.starts_with('"') {
            continue;
        }
        let after_open = &rest[1..];
        if let Some(end) = after_open.find('"') {
            return Ok(after_open[..end].to_string());
        }
    }
    Err("missing version line in formula".to_string())
}

fn result_from_fetch(
    current: &str,
    now: u64,
    fetched: Result<String, String>,
) -> UpdateCheckResult {
    match fetched {
        Ok(latest) => UpdateCheckResult {
            has_update: is_newer_version(current, &latest),
            current_version: current.to_string(),
            latest_version: Some(latest),
            checked_at: now,
            error: None,
        },
        Err(err) => UpdateCheckResult {
            has_update: false,
            current_version: current.to_string(),
            latest_version: None,
            checked_at: now,
            error: Some(err),
        },
    }
}

/// Fetch the latest version from both npm and Homebrew in parallel and
/// return a `MultiSourceUpdateResult`. Network failures are captured per
/// source so a flaky channel doesn't hide the working one.
pub async fn check_for_updates_all_sources(current_version: &str) -> MultiSourceUpdateResult {
    let now = now_ms();
    let (npm, homebrew) = tokio::join!(fetch_latest_version(), fetch_homebrew_latest_version());
    MultiSourceUpdateResult {
        current_version: current_version.to_string(),
        npm: result_from_fetch(current_version, now, npm),
        homebrew: result_from_fetch(current_version, now, homebrew),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_formula_version_picks_first_version_line() {
        let formula = r#"
class Wisetree < Formula
  desc "Worktree manager"
  homepage "https://example.com"
  version "1.2.3"
  license "MIT"
end
"#;
        assert_eq!(parse_formula_version(formula).unwrap(), "1.2.3");
    }

    #[test]
    fn parse_formula_version_ignores_keywords_that_start_with_version() {
        let formula = r#"
class Wisetree < Formula
  versioned_formula "ignored"
  version "9.9.9"
end
"#;
        assert_eq!(parse_formula_version(formula).unwrap(), "9.9.9");
    }

    #[test]
    fn parse_formula_version_errors_when_missing() {
        let formula = r#"
class Wisetree < Formula
  desc "no version here"
end
"#;
        assert!(parse_formula_version(formula).is_err());
    }

    #[test]
    fn update_source_upgrade_argv_uses_global_install_for_npm() {
        let argv = UpdateSource::Npm.upgrade_argv();
        assert_eq!(argv, &["npm", "install", "-g", "wisetree"]);
    }

    #[test]
    fn update_source_upgrade_argv_uses_tap_for_homebrew() {
        let argv = UpdateSource::Homebrew.upgrade_argv();
        assert_eq!(argv, &["brew", "upgrade", "victorcorcos/tap/wisetree"]);
    }
}

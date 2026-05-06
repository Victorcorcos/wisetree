//! `UpdateService` — best-effort npm-registry version check with a
//! 24-hour cache backed by `AppStateService`. Mirrors upstream's
//! `update-service.ts` (`shouldCheckForUpdates`, `checkForUpdates`,
//! `getCachedUpdateStatus`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::services::app_state::AppStateService;
use crate::utils::is_newer_version;

const NPM_REGISTRY_URL: &str = "https://registry.npmjs.org/wisetree/latest";
const CACHE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheckResult {
    pub has_update: bool,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub checked_at: u64,
    pub error: Option<String>,
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

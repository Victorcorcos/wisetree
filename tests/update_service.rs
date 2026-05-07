//! Tests for `services::update` cache logic. Network calls are not exercised
//! here; we validate the deterministic surface area (TTL gate, cached
//! status reconstruction).

use wisetree::services::app_state::AppStateService;
use wisetree::services::update::{
    get_cached_update_status, should_check_for_updates, UpdateCheckResult,
};

fn fresh() -> AppStateService {
    AppStateService::new()
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn should_check_when_no_prior_record() {
    let svc = fresh();
    assert!(should_check_for_updates(&svc));
}

#[test]
fn should_not_check_within_ttl() {
    let mut svc = fresh();
    svc.update(|s| s.last_update_check = Some(now_ms()));
    assert!(!should_check_for_updates(&svc));
}

#[test]
fn should_check_after_ttl_expires() {
    let mut svc = fresh();
    let stale = now_ms().saturating_sub(25 * 60 * 60 * 1000);
    svc.update(|s| s.last_update_check = Some(stale));
    assert!(should_check_for_updates(&svc));
}

#[test]
fn cached_status_returns_none_when_unset() {
    let svc = fresh();
    assert!(get_cached_update_status(&svc, Some("1.0.0")).is_none());
}

#[test]
fn cached_status_reflects_newer_when_latest_greater_than_current() {
    let mut svc = fresh();
    svc.update(|s| {
        s.last_update_check = Some(now_ms());
        s.latest_version = Some("2.0.0".into());
        s.checked_version = Some("1.0.0".into());
    });
    let r = get_cached_update_status(&svc, Some("1.0.0")).unwrap();
    assert!(r.has_update);
    assert_eq!(r.latest_version.as_deref(), Some("2.0.0"));
}

#[test]
fn cached_status_not_newer_when_current_at_or_above_latest() {
    let mut svc = fresh();
    svc.update(|s| {
        s.last_update_check = Some(now_ms());
        s.latest_version = Some("1.0.0".into());
    });
    let r = get_cached_update_status(&svc, Some("1.0.0")).unwrap();
    assert!(!r.has_update);
}

#[test]
fn update_check_result_is_constructible() {
    // Smoke test that the public type is reachable from external crates.
    let r = UpdateCheckResult {
        has_update: false,
        current_version: "1.0.0".into(),
        latest_version: None,
        checked_at: 0,
        error: None,
    };
    assert!(!r.has_update);
}

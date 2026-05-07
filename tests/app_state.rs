use std::fs;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use tempfile::TempDir;
use wisetree::config::AppState;
use wisetree::services::AppStateService;

static HOME_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

fn with_home<F: FnOnce(&TempDir)>(f: F) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", tmp.path());
    f(&tmp);
    if let Some(p) = prev {
        std::env::set_var("HOME", p);
    } else {
        std::env::remove_var("HOME");
    }
}

#[test]
fn missing_state_returns_defaults() {
    with_home(|_home| {
        let mut svc = AppStateService::new();
        let state = svc.load();
        assert_eq!(state, AppState::default());
    });
}

#[test]
fn corrupt_state_returns_defaults() {
    with_home(|home| {
        let dir = home.path().join(".wisetree");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("state.json"), "{not json").unwrap();

        let mut svc = AppStateService::new();
        let state = svc.load();
        assert_eq!(state, AppState::default());
    });
}

#[test]
fn save_then_load_round_trip() {
    with_home(|home| {
        let mut svc = AppStateService::new();
        let _ = svc.load();
        svc.update(|s| {
            s.last_update_check = Some(1_700_000_000_000);
            s.latest_version = Some("9.9.9".into());
            s.checked_version = Some("1.0.0".into());
        });
        svc.save();

        let path = home.path().join(".wisetree").join("state.json");
        let raw = fs::read_to_string(&path).expect("state file written");
        assert!(raw.contains("\"lastUpdateCheck\""));
        assert!(raw.contains("\"latestVersion\""));
        assert!(raw.contains("\"checkedVersion\""));

        let mut svc2 = AppStateService::new();
        let state = svc2.load();
        assert_eq!(state.last_update_check, Some(1_700_000_000_000));
        assert_eq!(state.latest_version.as_deref(), Some("9.9.9"));
        assert_eq!(state.checked_version.as_deref(), Some("1.0.0"));
    });
}

#[test]
fn save_skips_none_fields() {
    with_home(|home| {
        let mut svc = AppStateService::new();
        let _ = svc.load();
        svc.save();

        let path = home.path().join(".wisetree").join("state.json");
        let raw = fs::read_to_string(&path).expect("state file written");
        assert!(!raw.contains("lastUpdateCheck"));
        assert!(!raw.contains("latestVersion"));
        assert!(!raw.contains("checkedVersion"));
    });
}

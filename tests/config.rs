use std::fs;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use tempfile::TempDir;
use wisetree::config::schema::{
    default_copy_ignores, default_copy_patterns, default_path_template, LinkStrategy,
};
use wisetree::config::{ConfigService, WorktreeConfig};

/// Serialises tests that mutate `$HOME` so the global-config path resolution
/// is deterministic.
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
fn defaults_match_upstream() {
    let cfg = WorktreeConfig::default();
    assert_eq!(cfg.worktree_copy_patterns, default_copy_patterns());
    assert_eq!(cfg.worktree_copy_ignores, default_copy_ignores());
    assert_eq!(cfg.worktree_path_template, default_path_template());
    assert!(cfg.post_create_cmd.is_empty());
    assert!(cfg.worktree_link_patterns.is_empty());
    assert_eq!(cfg.worktree_link_strategy, LinkStrategy::CreateEmpty);
    assert_eq!(cfg.worktree_link_cache_dir, None);
    assert_eq!(cfg.terminal_command, "");
    assert!(!cfg.delete_branch_with_worktree);
}

#[test]
fn link_fields_round_trip() {
    let cfg = WorktreeConfig {
        worktree_link_patterns: vec!["node_modules".into(), "target".into()],
        worktree_link_strategy: LinkStrategy::SeedIfPresent,
        worktree_link_cache_dir: Some("$BASE_PATH/.cache/wisetree".into()),
        ..WorktreeConfig::default()
    };

    let raw = serde_json::to_string(&cfg).unwrap();
    let round_trip: WorktreeConfig = serde_json::from_str(&raw).unwrap();
    assert_eq!(round_trip, cfg);
}

#[test]
fn local_config_takes_precedence_over_global() {
    with_home(|home| {
        let project = tempfile::tempdir().expect("project tempdir");

        let local = WorktreeConfig {
            terminal_command: "from-local".into(),
            ..WorktreeConfig::default()
        };
        let local_json = serde_json::to_string_pretty(&local).unwrap();
        fs::write(project.path().join(".wisetree.json"), local_json).unwrap();

        let global_dir = home.path().join(".wisetree");
        fs::create_dir_all(&global_dir).unwrap();
        let global = WorktreeConfig {
            terminal_command: "from-global".into(),
            ..WorktreeConfig::default()
        };
        fs::write(
            global_dir.join("settings.json"),
            serde_json::to_string_pretty(&global).unwrap(),
        )
        .unwrap();

        let mut svc = ConfigService::new();
        let loaded = svc.load(Some(project.path())).expect("load");
        assert_eq!(loaded.terminal_command, "from-local");
    });
}

#[test]
fn falls_back_to_global_when_no_local() {
    with_home(|home| {
        let project = tempfile::tempdir().expect("project tempdir");

        let global_dir = home.path().join(".wisetree");
        fs::create_dir_all(&global_dir).unwrap();
        let global = WorktreeConfig {
            terminal_command: "from-global".into(),
            ..WorktreeConfig::default()
        };
        fs::write(
            global_dir.join("settings.json"),
            serde_json::to_string_pretty(&global).unwrap(),
        )
        .unwrap();

        let mut svc = ConfigService::new();
        let loaded = svc.load(Some(project.path())).expect("load");
        assert_eq!(loaded.terminal_command, "from-global");
    });
}

#[test]
fn load_global_ignores_local_config() {
    with_home(|home| {
        let project = tempfile::tempdir().expect("project tempdir");

        let local = WorktreeConfig {
            terminal_command: "from-local".into(),
            ..WorktreeConfig::default()
        };
        fs::write(
            project.path().join(".wisetree.json"),
            serde_json::to_string_pretty(&local).unwrap(),
        )
        .unwrap();

        let global_dir = home.path().join(".wisetree");
        fs::create_dir_all(&global_dir).unwrap();
        let global = WorktreeConfig {
            terminal_command: "from-global".into(),
            delete_branch_with_worktree: true,
            ..WorktreeConfig::default()
        };
        fs::write(
            global_dir.join("settings.json"),
            serde_json::to_string_pretty(&global).unwrap(),
        )
        .unwrap();

        let mut svc = ConfigService::new();
        let loaded = svc.load_global().expect("load global");
        assert_eq!(loaded.terminal_command, "from-global");
        assert!(loaded.delete_branch_with_worktree);
    });
}

#[test]
fn ensure_global_config_creates_dir_and_file() {
    with_home(|home| {
        let svc = ConfigService::new();
        svc.ensure_global_config().expect("ensure");
        let path = home.path().join(".wisetree").join("settings.json");
        assert!(
            path.exists(),
            "global config not created at {}",
            path.display()
        );

        let parsed: WorktreeConfig =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).expect("valid json");
        assert_eq!(parsed, WorktreeConfig::default());
    });
}

#[test]
fn save_writes_two_space_indent() {
    with_home(|home| {
        let project = tempfile::tempdir().expect("project tempdir");
        let mut svc = ConfigService::new();
        let _ = svc.load(Some(project.path())).expect("load");

        let cfg = WorktreeConfig {
            terminal_command: "code $WORKTREE_PATH".into(),
            ..WorktreeConfig::default()
        };
        let target = project.path().join(".wisetree.json");
        svc.save(&cfg, Some(&target)).expect("save");

        let raw = fs::read_to_string(&target).unwrap();
        assert!(
            raw.contains("\n  \"worktreeCopyPatterns\""),
            "expected 2-space indent: {raw}"
        );
        assert!(raw.contains("\"terminalCommand\": \"code $WORKTREE_PATH\""));
        let _ = home;
    });
}

#[test]
fn malformed_local_config_returns_error_with_path() {
    with_home(|home| {
        let project = tempfile::tempdir().expect("project tempdir");
        let path = project.path().join(".wisetree.json");
        fs::write(&path, "{ not valid json").unwrap();

        let mut svc = ConfigService::new();
        let err = svc.load(Some(project.path())).expect_err("must error");
        let msg = format!("{err}");
        assert!(msg.contains(".wisetree.json"), "{msg}");
        let _ = home;
    });
}

#[test]
fn missing_config_returns_defaults_with_no_error() {
    with_home(|_home| {
        let project = tempfile::tempdir().expect("project tempdir");
        let mut svc = ConfigService::new();
        let cfg = svc.load(Some(project.path())).expect("load");
        // ensure_global_config wrote defaults; loaded path now points there.
        assert_eq!(cfg, WorktreeConfig::default());
    });
}

#[test]
fn unknown_field_rejected() {
    let raw = r#"{"unknownField": 1}"#;
    let parsed: Result<WorktreeConfig, _> = serde_json::from_str(raw);
    assert!(parsed.is_err(), "deny_unknown_fields should reject");
}

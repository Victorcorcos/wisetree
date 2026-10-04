use std::fs;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use tempfile::TempDir;
use wisetree::config::schema::{clamp_dashboard_refresh_interval, LinkStrategy};
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

/// Path to a project's config in its current home, creating `.wisetree/` so a
/// plain `fs::write` succeeds.
fn project_config(root: &std::path::Path) -> std::path::PathBuf {
    let path = root.join(".wisetree").join("config.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    path
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
        fs::write(project_config(project.path()), local_json).unwrap();

        let global_dir = home.path().join(".wisetree");
        fs::create_dir_all(&global_dir).unwrap();
        let global = WorktreeConfig {
            terminal_command: "from-global".into(),
            ..WorktreeConfig::default()
        };
        fs::write(
            global_dir.join("config.json"),
            serde_json::to_string_pretty(&global).unwrap(),
        )
        .unwrap();

        let mut svc = ConfigService::new();
        let loaded = svc.load(Some(project.path())).expect("load");
        assert_eq!(loaded.terminal_command, "from-local");
    });
}

#[test]
fn mother_config_takes_precedence_over_child_then_global() {
    with_home(|home| {
        let mother = tempfile::tempdir().expect("mother tempdir");
        let child = tempfile::tempdir().expect("child tempdir");
        let mother_path = project_config(mother.path());
        let child_path = project_config(child.path());
        let global_path = home.path().join(".wisetree").join("config.json");
        fs::create_dir_all(global_path.parent().unwrap()).unwrap();

        for (path, terminal_command) in [
            (&mother_path, "from-mother"),
            (&child_path, "from-child"),
            (&global_path, "from-global"),
        ] {
            let config = WorktreeConfig {
                terminal_command: terminal_command.into(),
                ..WorktreeConfig::default()
            };
            fs::write(path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
        }

        let mut svc = ConfigService::new();
        let loaded = svc
            .load_for_worktree(mother.path(), child.path())
            .expect("load mother config");
        assert_eq!(loaded.terminal_command, "from-mother");
        assert_eq!(svc.config_path(), Some(mother_path.as_path()));

        fs::remove_file(&mother_path).unwrap();
        let loaded = svc
            .load_for_worktree(mother.path(), child.path())
            .expect("load child config");
        assert_eq!(loaded.terminal_command, "from-child");
        assert_eq!(svc.config_path(), Some(child_path.as_path()));

        fs::remove_file(&child_path).unwrap();
        let loaded = svc
            .load_for_worktree(mother.path(), child.path())
            .expect("load global config");
        assert_eq!(loaded.terminal_command, "from-global");
        assert_eq!(svc.config_path(), Some(global_path.as_path()));
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
            global_dir.join("config.json"),
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
            project_config(project.path()),
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
            global_dir.join("config.json"),
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
        let path = home.path().join(".wisetree").join("config.json");
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
        let target = project_config(project.path());
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
        let path = project_config(project.path());
        fs::write(&path, "{ not valid json").unwrap();

        let mut svc = ConfigService::new();
        let err = svc.load(Some(project.path())).expect_err("must error");
        let msg = format!("{err}");
        assert!(msg.contains(".wisetree/config.json"), "{msg}");
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
fn unknown_field_is_ignored() {
    let raw = r#"{"unknownField": 1}"#;
    let parsed: Result<WorktreeConfig, _> = serde_json::from_str(raw);
    assert!(parsed.is_ok(), "legacy fields should be ignored");
}

#[test]
fn dashboard_refresh_interval_is_clamped_on_load() {
    with_home(|home| {
        let project = tempfile::tempdir().expect("project tempdir");
        let raw = r#"{
  "dashboard": {
    "refreshIntervalMs": 10
  }
}"#;
        fs::write(project_config(project.path()), raw).unwrap();

        let mut svc = ConfigService::new();
        let loaded = svc.load(Some(project.path())).expect("load");
        assert_eq!(loaded.dashboard.refresh_interval_ms, 5_000);

        let global_dir = home.path().join(".wisetree");
        fs::create_dir_all(&global_dir).unwrap();
    });
    assert_eq!(clamp_dashboard_refresh_interval(100_000), 60_000);
}

#[test]
fn dashboard_unknown_field_is_ignored() {
    let raw = r#"{
  "dashboard": {
    "bogus": true
  }
}"#;
    let parsed: Result<WorktreeConfig, _> = serde_json::from_str(raw);
    assert!(parsed.is_ok(), "legacy dashboard fields should be ignored");
}

#[test]
fn saving_legacy_config_keeps_only_basic_fields_in_both_scopes() {
    with_home(|home| {
        let project = tempfile::tempdir().expect("project");
        let local = project_config(project.path());
        let global = home.path().join(".wisetree/config.json");
        fs::create_dir_all(global.parent().unwrap()).unwrap();
        let raw = r#"{
            "legacyOption": {"enabled": true},
            "terminalCommand": "code $WORKTREE_PATH",
            "deleteBranchWithWorktree": true,
            "dashboard": {
                "legacyOption": true,
                "refreshIntervalMs": 7000,
                "showPullRequests": true,
                "columns": ["branch", "obsolete_column", "status"]
            }
        }"#;
        for target in [&local, &global] {
            fs::write(target, raw).unwrap();
            let mut service = ConfigService::new();
            let mut config = if target == &local {
                service.load(Some(project.path())).unwrap()
            } else {
                service.load_global().unwrap()
            };
            assert_eq!(fs::read_to_string(target).unwrap(), raw);
            assert_eq!(config.dashboard.columns, vec!["branch", "status"]);
            config.dashboard.refresh_interval_ms = 8000;
            service.save(&config, None).unwrap();
            let saved: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(target).unwrap()).unwrap();
            assert!(saved.get("legacyOption").is_none());
            assert!(saved["dashboard"].get("legacyOption").is_none());
            assert_eq!(saved["terminalCommand"], "code $WORKTREE_PATH");
            assert_eq!(saved["deleteBranchWithWorktree"], true);
            assert_eq!(saved["dashboard"]["refreshIntervalMs"], 8000);
            assert_eq!(saved["dashboard"]["showPullRequests"], true);
        }
    });
}

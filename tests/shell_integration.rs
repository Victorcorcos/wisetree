//! Tests for the shell-integration service. Each test runs against a
//! tempdir-rooted `$HOME` so the user's real rc files are never touched.
//! `$HOME` is process-global, so tests are serialized via `ENV_LOCK`.

use std::fs;
use std::sync::{Mutex, MutexGuard};

use tempfile::TempDir;
use wisetree::services::shell_integration::{
    detect_shell_integration_with, find_setup_end_index, generate_setup_block,
    install_shell_integration, remove_shell_integration, Shell,
};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn isolated_home() -> TempDir {
    let home = TempDir::new().unwrap();
    std::env::set_var("HOME", home.path());
    home
}

#[test]
fn generate_setup_block_for_zsh_includes_signature_and_marker() {
    let block = generate_setup_block(Shell::Zsh, "wisetree", "2026-05-05".into());
    assert!(block.contains("# Wisetree setup: added on 2026-05-05"));
    assert!(block.contains("# End Wisetree setup"));
    assert!(block.contains("compdef _wisetree wisetree"));
    assert!(block.contains("FORCE_COLOR=3 command wisetree --from-wrapper"));
    assert!(block.contains("Wisetree: Navigated to"));
}

#[test]
fn generate_setup_block_for_bash_includes_completions() {
    let block = generate_setup_block(Shell::Bash, "wisetree", "2026-05-05".into());
    assert!(block.contains("_wisetree_completions"));
    assert!(block.contains("create list delete settings"));
    assert!(block.contains("--help --version --mode --from-wrapper"));
    assert!(block.contains("menu create list delete settings"));
}

#[test]
fn install_creates_zshrc_when_missing() {
    let _g = lock_env();
    let _home = isolated_home();
    install_shell_integration(Shell::Zsh, "wisetree").unwrap();
    let zshrc_path = std::env::var("HOME")
        .map(|h| format!("{h}/.zshrc"))
        .unwrap();
    let content = fs::read_to_string(&zshrc_path).unwrap();
    assert!(content.contains("# Wisetree setup: added on"));
    assert!(content.contains("# End Wisetree setup"));
}

#[test]
fn install_replaces_existing_block() {
    let _g = lock_env();
    let home = isolated_home();
    let zshrc_path = home.path().join(".zshrc");
    fs::write(
        &zshrc_path,
        "# user content\n\
         # Wisetree setup: added on 2025-01-01\n\
         old setup\n\
         # End Wisetree setup\n\
         # other content\n",
    )
    .unwrap();

    install_shell_integration(Shell::Zsh, "wisetree").unwrap();
    let content = fs::read_to_string(&zshrc_path).unwrap();
    assert!(!content.contains("old setup"));
    assert!(content.contains("# user content"));
    assert!(content.contains("# other content"));
    assert_eq!(
        content.matches("# Wisetree setup: added on").count(),
        1,
        "should have exactly one signature after re-install"
    );
}

#[test]
fn remove_strips_block_and_surrounding_blank_lines() {
    let _g = lock_env();
    let home = isolated_home();
    let zshrc_path = home.path().join(".zshrc");
    fs::write(
        &zshrc_path,
        "alias x=1\n\
         \n\
         # Wisetree setup: added on 2025-01-01\n\
         body\n\
         # End Wisetree setup\n\
         \n\
         alias y=2\n",
    )
    .unwrap();
    remove_shell_integration(Shell::Zsh).unwrap();
    let content = fs::read_to_string(&zshrc_path).unwrap();
    assert!(!content.contains("Wisetree"));
    assert!(content.contains("alias x=1"));
    assert!(content.contains("alias y=2"));
}

#[test]
fn detect_returns_installed_when_signature_present() {
    let _g = lock_env();
    let home = isolated_home();
    fs::write(
        home.path().join(".zshrc"),
        "# Wisetree setup: added on 2025-01-01\nbody\n# End Wisetree setup\n",
    )
    .unwrap();
    let status = detect_shell_integration_with(Shell::Zsh);
    assert!(status.is_installed);
    assert_eq!(status.shell, Shell::Zsh);
}

#[test]
fn detect_returns_not_installed_when_signature_missing() {
    let _g = lock_env();
    let home = isolated_home();
    fs::write(home.path().join(".zshrc"), "alias x=1\n").unwrap();
    let status = detect_shell_integration_with(Shell::Zsh);
    assert!(!status.is_installed);
    assert!(status.reason.is_some());
}

#[test]
fn detect_handles_unknown_shell() {
    let _g = lock_env();
    let _home = isolated_home();
    let status = detect_shell_integration_with(Shell::Unknown);
    assert!(!status.is_installed);
    assert!(status.config_path.is_none());
}

#[test]
fn find_end_index_uses_marker_when_present() {
    let lines = vec![
        "user line",
        "# Wisetree setup: added on 2025-01-01",
        "body",
        "# End Wisetree setup",
        "more",
    ];
    let end = find_setup_end_index(&lines, 1);
    assert_eq!(end, 3);
}

#[test]
fn find_end_index_falls_back_to_closing_brace() {
    let lines = vec![
        "# Wisetree setup: added on 2025-01-01",
        "wisetree() {",
        "  body",
        "}",
        "more",
    ];
    let end = find_setup_end_index(&lines, 0);
    assert_eq!(end, 3);
}

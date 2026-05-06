//! State-machine + render tests for the Settings screen.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use wisetree::config::schema::WorktreeConfig;
use wisetree::services::UpdateCheckResult;
use wisetree::tui::screens::settings::{SettingsAction, SettingsScreen, SettingsStep};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn dump<F>(width: u16, height: u16, draw: F) -> String
where
    F: FnOnce(&mut ratatui::Frame),
{
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(draw).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect()
}

fn ready() -> SettingsScreen {
    let cfg = WorktreeConfig {
        post_create_cmd: vec!["bun install".into()],
        terminal_command: "code $WORKTREE_PATH".into(),
        delete_branch_with_worktree: true,
        ..Default::default()
    };
    SettingsScreen::new(cfg, "/tmp/.wisetree.json".into())
}

#[test]
fn menu_renders_with_config_path() {
    let s = ready();
    let dumped = dump(80, 12, |f| s.render(f, f.area()));
    assert!(dumped.contains("Configuration file"));
    assert!(dumped.contains("/tmp/.wisetree.json"));
    assert!(dumped.contains("Copy Patterns"));
    assert!(dumped.contains("Check for Updates"));
}

#[test]
fn esc_on_menu_returns_back() {
    let mut s = ready();
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, SettingsAction::Back);
}

#[test]
fn selecting_copy_patterns_shows_detail_view() {
    let mut s = ready();
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), SettingsStep::CopyPatterns);
    let dumped = dump(80, 12, |f| s.render(f, f.area()));
    assert!(dumped.contains("Copy Patterns"));
    assert!(dumped.contains(".env*"));
}

#[test]
fn any_key_in_detail_returns_to_menu() {
    let mut s = ready();
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), SettingsStep::CopyPatterns);
    let action = s.handle_key(key(KeyCode::Char('x')));
    assert_eq!(action, SettingsAction::Continue);
    assert_eq!(s.step(), SettingsStep::Menu);
}

#[test]
fn delete_branch_setting_renders_yes_no_toggle() {
    let mut s = ready();
    for _ in 0..5 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), SettingsStep::DeleteBranch);

    let dumped = dump(80, 14, |f| s.render(f, f.area()));
    assert!(dumped.contains("Delete Branch with Worktree"));
    assert!(dumped.contains("Yes"));
    assert!(dumped.contains("No"));
    assert!(dumped.contains("Never deletes current or default branches"));
}

#[test]
fn delete_branch_setting_emits_true_when_yes_selected() {
    let mut s = ready();
    for _ in 0..5 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SettingsAction::SetDeleteBranchWithWorktree(true));
}

#[test]
fn delete_branch_setting_emits_false_when_no_selected() {
    let mut s = ready();
    for _ in 0..5 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('n')));

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SettingsAction::SetDeleteBranchWithWorktree(false));
}

#[test]
fn select_check_updates_emits_action() {
    let mut s = ready();
    // Navigate to last entry "Check for Updates" — 6 downs from the first.
    for _ in 0..6 {
        s.handle_key(key(KeyCode::Down));
    }
    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SettingsAction::CheckUpdates);
    assert_eq!(s.step(), SettingsStep::CheckUpdates);
}

#[test]
fn check_updates_loading_renders_spinner_message() {
    let mut s = ready();
    for _ in 0..6 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    s.start_checking_updates();
    let dumped = dump(60, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("Checking for updates"));
}

#[test]
fn check_updates_with_new_version_shows_install_command() {
    let mut s = ready();
    for _ in 0..6 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    s.start_checking_updates();
    s.set_update_result(UpdateCheckResult {
        has_update: true,
        current_version: "1.0.0".into(),
        latest_version: Some("2.0.0".into()),
        checked_at: 0,
        error: None,
    });
    let dumped = dump(80, 8, |f| s.render(f, f.area()));
    assert!(dumped.contains("New version available"));
    assert!(dumped.contains("v2.0.0"));
    assert!(dumped.contains("npm install -g wisetree"));
}

#[test]
fn check_updates_up_to_date_message() {
    let mut s = ready();
    for _ in 0..6 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    s.set_update_result(UpdateCheckResult {
        has_update: false,
        current_version: "1.2.0".into(),
        latest_version: Some("1.2.0".into()),
        checked_at: 0,
        error: None,
    });
    let dumped = dump(80, 6, |f| s.render(f, f.area()));
    assert!(dumped.contains("latest version"));
    assert!(dumped.contains("v1.2.0"));
}

#[test]
fn check_updates_error_shows_failure_message() {
    let mut s = ready();
    for _ in 0..6 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    s.set_update_result(UpdateCheckResult {
        has_update: false,
        current_version: "1.2.0".into(),
        latest_version: None,
        checked_at: 0,
        error: Some("network down".into()),
    });
    let dumped = dump(80, 6, |f| s.render(f, f.area()));
    assert!(dumped.contains("Failed to check for updates"));
}

#[test]
fn error_overlay_with_r_emits_reset() {
    let mut s = ready();
    s.set_error("boom".into());
    let action = s.handle_key(key(KeyCode::Char('r')));
    assert_eq!(action, SettingsAction::Reset);
}

#[test]
fn error_overlay_clears_on_other_key_and_returns_back() {
    let mut s = ready();
    s.set_error("boom".into());
    let action = s.handle_key(key(KeyCode::Char('x')));
    assert_eq!(action, SettingsAction::Back);
    assert!(s.error().is_none());
}

//! State-machine + render tests for the Setup Shell Integration screen.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use wisetree::services::shell_integration::{Shell, ShellIntegrationStatus};
use wisetree::tui::screens::setup::{SetupAction, SetupScreen, SetupStep};

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

fn status(shell: Shell) -> ShellIntegrationStatus {
    ShellIntegrationStatus {
        is_installed: false,
        shell,
        config_path: None,
        reason: None,
    }
}

#[test]
fn select_shell_renders_intro_and_options() {
    let s = SetupScreen::new(Some(&status(Shell::Zsh)));
    let dumped = dump(80, 8, |f| s.render(f, f.area()));
    assert!(dumped.contains("Shell integration wraps"));
    assert!(dumped.contains("zsh"));
    assert!(dumped.contains("bash"));
    assert!(dumped.contains("detected"));
}

#[test]
fn enter_advances_to_confirm() {
    let mut s = SetupScreen::new(Some(&status(Shell::Zsh)));
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), SetupStep::Confirm);
    assert_eq!(s.selected_shell(), Shell::Zsh);
}

#[test]
fn esc_in_select_cancels() {
    let mut s = SetupScreen::new(Some(&status(Shell::Zsh)));
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, SetupAction::Cancelled);
}

#[test]
fn confirm_renders_preview_with_wisetree_function() {
    let mut s = SetupScreen::new(Some(&status(Shell::Zsh)));
    s.handle_key(key(KeyCode::Enter));
    let dumped = dump(80, 24, |f| s.render(f, f.area()));
    assert!(dumped.contains("Install Shell Integration"));
    assert!(dumped.contains("wisetree"));
    assert!(dumped.contains("FORCE_COLOR=3"));
}

#[test]
fn confirm_yes_emits_confirmed() {
    let mut s = SetupScreen::new(Some(&status(Shell::Zsh)));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('y')));
    let action = s.handle_key(key(KeyCode::Enter));
    match action {
        SetupAction::Confirmed { shell } => assert_eq!(shell, Shell::Zsh),
        other => panic!("expected Confirmed, got {other:?}"),
    }
}

#[test]
fn esc_in_confirm_returns_to_select() {
    let mut s = SetupScreen::new(Some(&status(Shell::Zsh)));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Esc));
    assert_eq!(s.step(), SetupStep::SelectShell);
}

#[test]
fn installing_renders_spinner() {
    let mut s = SetupScreen::new(Some(&status(Shell::Zsh)));
    s.start_installing();
    let dumped = dump(60, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("Installing shell integration"));
}

#[test]
fn success_renders_message_and_done_on_enter() {
    let mut s = SetupScreen::new(Some(&status(Shell::Zsh)));
    s.handle_key(key(KeyCode::Enter)); // -> confirm
    s.handle_key(key(KeyCode::Char('y')));
    s.handle_key(key(KeyCode::Enter)); // confirmed
    s.start_installing();
    s.mark_complete();
    let dumped = dump(80, 8, |f| s.render(f, f.area()));
    assert!(dumped.contains("Shell integration installed successfully"));
    assert!(dumped.contains("source ~/.zshrc"));
    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SetupAction::Done);
}

#[test]
fn error_state_renders_message() {
    let mut s = SetupScreen::new(Some(&status(Shell::Zsh)));
    s.set_error("permission denied".into());
    assert_eq!(s.step(), SetupStep::Errored);
    let dumped = dump(80, 6, |f| s.render(f, f.area()));
    assert!(dumped.contains("Failed to install"));
    assert!(dumped.contains("permission denied"));
    let action = s.handle_key(key(KeyCode::Char('x')));
    assert_eq!(action, SetupAction::Cancelled);
}

#[test]
fn bash_default_selection_when_detected() {
    let s = SetupScreen::new(Some(&status(Shell::Bash)));
    let dumped = dump(80, 6, |f| s.render(f, f.area()));
    // The "detected" tag attached to the bash entry should be visible.
    assert!(dumped.contains("bash"));
    assert!(dumped.contains("detected"));
}

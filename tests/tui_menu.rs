//! Behavior + render tests for the main menu screen.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use wisetree::tui::screens::menu::{MenuChoice, MenuOutcome, MenuScreen};
use wisetree::tui::widgets::welcome_header::fold_home;

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

#[test]
fn menu_default_shows_five_entries_when_shell_status_unknown() {
    let menu = MenuScreen::new(0, None, None);
    assert!(!menu.has_setup_entry());
    let dumped = dump(80, 18, |f| menu.render(f, f.area()));
    assert!(dumped.contains("Create new worktree"));
    assert!(dumped.contains("List worktrees"));
    assert!(dumped.contains("Delete worktree"));
    assert!(dumped.contains("Settings"));
    assert!(dumped.contains("Exit"));
    assert!(!dumped.contains("Setup Shell Integration"));
}

#[test]
fn menu_shows_setup_entry_when_shell_not_installed() {
    let menu = MenuScreen::new(0, None, Some(false));
    assert!(menu.has_setup_entry());
    let dumped = dump(80, 20, |f| menu.render(f, f.area()));
    assert!(dumped.contains("Setup Shell Integration"));
    assert!(dumped.contains("recommended"));
}

#[test]
fn menu_hides_setup_when_shell_already_installed() {
    let menu = MenuScreen::new(0, None, Some(true));
    assert!(!menu.has_setup_entry());
    let dumped = dump(80, 20, |f| menu.render(f, f.area()));
    assert!(!dumped.contains("Setup Shell Integration"));
}

#[test]
fn menu_enter_dispatches_choice_for_create() {
    let mut menu = MenuScreen::new(0, None, None);
    match menu.handle_key(key(KeyCode::Enter)) {
        MenuOutcome::Selected(MenuChoice::Create, idx) => assert_eq!(idx, 0),
        _ => panic!("expected Create"),
    }
}

#[test]
fn menu_arrow_navigation_then_enter_picks_settings() {
    let mut menu = MenuScreen::new(0, None, None);
    menu.handle_key(key(KeyCode::Down));
    menu.handle_key(key(KeyCode::Down));
    menu.handle_key(key(KeyCode::Down));
    match menu.handle_key(key(KeyCode::Enter)) {
        MenuOutcome::Selected(MenuChoice::Settings, _) => {}
        other => panic!("expected Settings, got {:?}", as_choice(other)),
    }
}

fn as_choice(o: MenuOutcome) -> Option<MenuChoice> {
    match o {
        MenuOutcome::Selected(c, _) => Some(c),
        _ => None,
    }
}

#[test]
fn menu_esc_cancels_to_quit() {
    let mut menu = MenuScreen::new(0, None, None);
    matches!(menu.handle_key(key(KeyCode::Esc)), MenuOutcome::Cancelled);
}

#[test]
fn menu_setup_entry_is_first_when_present() {
    let mut menu = MenuScreen::new(0, None, Some(false));
    match menu.handle_key(key(KeyCode::Enter)) {
        MenuOutcome::Selected(MenuChoice::Setup, idx) => assert_eq!(idx, 0),
        _ => panic!("expected Setup"),
    }
}

#[test]
fn menu_default_index_clamped_when_no_setup() {
    let menu = MenuScreen::new(99, None, None);
    assert!(menu.selected_index() < 5);
}

#[test]
fn menu_render_includes_welcome_header_and_cwd() {
    let menu = MenuScreen::new(0, Some("/tmp/repo".into()), None);
    let dumped = dump(80, 16, |f| menu.render(f, f.area()));
    assert!(dumped.contains("Welcome to"));
    assert!(dumped.contains("Wisetree"));
    assert!(dumped.contains("/tmp/repo"));
}

// -- WelcomeHeader fold_home -------------------------------------------------

#[test]
fn fold_home_substitutes_home_prefix_with_tilde() {
    std::env::set_var("HOME", "/Users/me");
    assert_eq!(fold_home("/Users/me/code/wisetree"), "~/code/wisetree");
    assert_eq!(fold_home("/Users/me"), "~");
    assert_eq!(fold_home("/var/log"), "/var/log");
}

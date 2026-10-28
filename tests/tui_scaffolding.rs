//! Unit-style tests for the TUI scaffolding: render-snapshot of the loading
//! and error screens against a `TestBackend`, plus router mapping and the
//! panic-hook restoration contract.

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use wisetree::cli::AppMode;
use wisetree::tui::router::Screen;
use wisetree::tui::screens::menu::MenuScreen;
use wisetree::tui::screens::{error as error_screen, loading as loading_screen};

#[test]
fn screen_from_mode_maps_every_variant() {
    assert_eq!(Screen::from_mode(AppMode::Menu), Screen::Menu);
    assert_eq!(Screen::from_mode(AppMode::Create), Screen::Create);
    assert_eq!(Screen::from_mode(AppMode::Dashboard), Screen::Dashboard);
    assert_eq!(Screen::from_mode(AppMode::Settings), Screen::Settings);
}

#[test]
fn screen_as_str_round_trip_for_known_modes() {
    for s in [
        Screen::Menu,
        Screen::Create,
        Screen::Dashboard,
        Screen::Settings,
    ] {
        let parsed = AppMode::parse(s.as_str()).expect("valid mode");
        assert_eq!(Screen::from_mode(parsed), s);
    }
    // Setup is reachable only via the menu, so AppMode has no entry.
    assert_eq!(Screen::Setup.as_str(), "setup");
    assert!(AppMode::parse("setup").is_none());
    // Delete is now internal-only (dashboard), so AppMode has no entry.
    assert_eq!(Screen::Delete.as_str(), "delete");
    assert!(AppMode::parse("delete").is_none());
}

#[test]
fn loading_screen_renders_spinner_and_mode() {
    let backend = TestBackend::new(40, 5);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| loading_screen::draw(f, f.area(), 0, "menu"))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let dump = buffer
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(dump.contains("Loading git information"));
    assert!(dump.contains("(menu)"));
}

#[test]
fn loading_screen_advances_through_all_spinner_frames() {
    let frames = loading_screen::spinner_frames();
    assert_eq!(frames.len(), 10);
    let unique: std::collections::HashSet<_> = frames.iter().copied().collect();
    assert_eq!(unique.len(), 10, "spinner frames must be distinct");
}

#[test]
fn error_screen_includes_message_and_reset_hint() {
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            error_screen::draw(
                f,
                f.area(),
                "Current directory is not a git repository.",
                false,
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let dump = buffer
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(dump.contains("Current directory is not a git repository"));
    assert!(dump.contains("Press 'r' to reset"));
}

#[test]
fn error_screen_with_reset_confirm_shows_yes_no_prompt() {
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| error_screen::draw(f, f.area(), "boom", true))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let dump = buffer
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(dump.contains("Reset configuration to defaults"));
    assert!(dump.contains("(y) Yes"));
    assert!(dump.contains("(n) No"));
}

#[test]
fn menu_placeholder_renders_welcome_and_prompt() {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| MenuScreen::new(0, None, None).render(f, f.area()))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let dump = buffer
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(dump.contains("Wisetree"));
    assert!(dump.contains("Choose wisely..."));
}

#[test]
fn install_panic_hook_chains_through_previous_hook() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    let flag = Arc::new(AtomicBool::new(false));
    let flag_clone = flag.clone();

    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |_| {
        flag_clone.store(true, Ordering::SeqCst);
    }));

    wisetree::tui::terminal::install_panic_hook();

    let result = std::panic::catch_unwind(|| panic!("boom"));
    assert!(result.is_err());
    assert!(
        flag.load(Ordering::SeqCst),
        "panic hook chain must reach the previous hook"
    );

    std::panic::set_hook(prev);
}

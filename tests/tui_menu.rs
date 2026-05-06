//! Behavior + render tests for the main menu screen.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::Terminal;

use wisetree::messages::colors;
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
    render(width, height, draw)
        .content
        .iter()
        .map(|c| c.symbol())
        .collect()
}

fn render<F>(width: u16, height: u16, draw: F) -> Buffer
where
    F: FnOnce(&mut ratatui::Frame),
{
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(draw).unwrap();
    terminal.backend().buffer().clone()
}

fn find_text_start(buffer: &Buffer, text: &str) -> Option<(u16, u16)> {
    let needle: Vec<String> = text.chars().map(|ch| ch.to_string()).collect();
    let width = buffer.area.width;
    let needle_len = needle.len() as u16;
    if needle_len == 0 || needle_len > width {
        return None;
    }

    for y in 0..buffer.area.height {
        for x in 0..=width - needle_len {
            if needle.iter().enumerate().all(|(offset, expected)| {
                buffer[(x + offset as u16, y)].symbol() == expected.as_str()
            }) {
                return Some((x, y));
            }
        }
    }

    None
}

fn assert_text_style(buffer: &Buffer, text: &str, fg: Color, bg: Color) {
    let (x, y) = find_text_start(buffer, text).unwrap_or_else(|| panic!("{text:?} not found"));

    for (offset, _) in text.chars().enumerate() {
        let cell = &buffer[(x + offset as u16, y)];
        assert_eq!(cell.fg, fg, "unexpected fg for {text:?} at offset {offset}");
        assert_eq!(cell.bg, bg, "unexpected bg for {text:?} at offset {offset}");
    }
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

#[test]
fn menu_render_applies_mockup_palette_to_header_menu_and_footer() {
    let menu = MenuScreen::new(0, Some("/tmp/repo".into()), None);
    let buffer = render(80, 20, |f| menu.render(f, f.area()));

    // "Welcome to " stays in the header title color, but "Wisetree" must
    // wear the brand purple per `design/pallete.md`.
    assert_text_style(
        &buffer,
        "Welcome to ",
        colors::HEADER_TITLE,
        colors::HEADER_BG,
    );
    assert_text_style(&buffer, "Wisetree", colors::BRAND, colors::HEADER_BG);
    assert_text_style(&buffer, "/tmp/repo", colors::MENU_TEXT, colors::HEADER_BG);
    // Titles like "Choose wisely..." use the teal info color.
    assert_text_style(
        &buffer,
        "Choose wisely...",
        colors::INFO,
        colors::MENU_BG,
    );
    assert_text_style(
        &buffer,
        "Create new worktree",
        colors::MENU_SELECTION_FG,
        colors::MENU_SELECTION_BG,
    );
    // The non-selected "List worktrees" row keeps the menu body color
    // for the verb but applies the brand purple to the noun.
    assert_text_style(&buffer, "List ", colors::MENU_TEXT, colors::MENU_BG);
    assert_text_style(&buffer, "worktrees", colors::BRAND, colors::MENU_BG);
    assert_text_style(&buffer, "Nav", colors::MENU_TEXT, colors::STATUS_BG);
    assert_text_style(
        &buffer,
        "Active Repo:",
        colors::HEADER_SUBTITLE,
        colors::STATUS_BG,
    );
}

#[test]
fn menu_render_indents_current_repository_line_inside_header() {
    let menu = MenuScreen::new(0, Some("/tmp/repo".into()), None);
    let buffer = render(80, 20, |f| menu.render(f, f.area()));
    let (x, y) =
        find_text_start(&buffer, "Current Repository").expect("current repository line present");

    assert_eq!(buffer[(x - 1, y)].symbol(), " ");
    assert_eq!(buffer[(x - 2, y)].symbol(), " ");
}

#[test]
fn menu_render_uses_rounded_selected_row_with_arrow_cursor() {
    let menu = MenuScreen::new(0, Some("/tmp/repo".into()), None);
    let dumped = dump(80, 20, |f| menu.render(f, f.area()));

    assert!(dumped.contains("➤"));
    assert!(!dumped.contains("◑"));
    assert!(!dumped.contains("◐"));
    assert_eq!(dumped.matches('╭').count(), 2);
    assert_eq!(dumped.matches('╮').count(), 2);
    assert_eq!(dumped.matches('╰').count(), 2);
    assert_eq!(dumped.matches('╯').count(), 2);
}

// -- WelcomeHeader fold_home -------------------------------------------------

#[test]
fn fold_home_substitutes_home_prefix_with_tilde() {
    std::env::set_var("HOME", "/Users/me");
    assert_eq!(fold_home("/Users/me/code/wisetree"), "~/code/wisetree");
    assert_eq!(fold_home("/Users/me"), "~");
    assert_eq!(fold_home("/var/log"), "/var/log");
}

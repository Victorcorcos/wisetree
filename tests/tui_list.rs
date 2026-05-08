//! State-machine + render tests for the List Worktrees screen.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use wisetree::git::types::GitWorktree;
use wisetree::tui::screens::list::{ListAction, ListScreen};

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

fn wt(path: &str, branch: &str, is_main: bool) -> GitWorktree {
    GitWorktree {
        path: path.into(),
        branch: branch.into(),
        commit: "deadbeef".into(),
        is_main,
        is_clean: true,
        branch_status: None,
    }
}

fn worktrees() -> Vec<GitWorktree> {
    vec![
        wt("/tmp/repo", "main", true),
        wt("/tmp/repo-feat", "feat", false),
        wt("/tmp/repo-bug", "bug", false),
    ]
}

#[test]
fn loading_render_shows_loading_message() {
    let s = ListScreen::new(true, true);
    let dumped = dump(60, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("Loading worktrees"));
}

#[test]
fn set_worktrees_filters_main_and_clears_loading() {
    let mut s = ListScreen::new(true, true);
    s.set_worktrees(worktrees());
    assert!(!s.loading());
    assert_eq!(s.worktrees().len(), 2);
    assert!(s.worktrees().iter().all(|w| !w.is_main));
}

#[test]
fn empty_list_shows_no_worktrees_and_back_on_keypress() {
    let mut s = ListScreen::new(true, true);
    s.set_worktrees(vec![wt("/tmp/repo", "main", true)]);
    let dumped = dump(60, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("No additional worktrees"));
    let action = s.handle_key(key(KeyCode::Char('x')));
    assert_eq!(action, ListAction::Back);
}

#[test]
fn down_arrow_navigates_with_wrap() {
    let mut s = ListScreen::new(true, true);
    s.set_worktrees(worktrees());
    assert_eq!(s.selected_index(), 0);
    s.handle_key(key(KeyCode::Down));
    assert_eq!(s.selected_index(), 1);
    s.handle_key(key(KeyCode::Down));
    assert_eq!(s.selected_index(), 0);
}

#[test]
fn typing_filters_worktrees_and_enter_selects_match() {
    // The list mirrors the Source-branch screen: typing filters the rows
    // (so j/k/digits/letters all feed the search query rather than acting
    // as legacy shortcuts), and Enter opens the action menu for the first
    // remaining match.
    let mut s = ListScreen::new(true, true);
    s.set_worktrees(worktrees());
    for ch in "bug".chars() {
        s.handle_key(key(KeyCode::Char(ch)));
    }
    // The action menu opens for the matching worktree's branch.
    s.handle_key(key(KeyCode::Enter));
    let action = s.handle_key(key(KeyCode::Enter));
    match action {
        ListAction::NavigateTo(path) => assert_eq!(path, "/tmp/repo-bug"),
        other => panic!("expected NavigateTo, got {other:?}"),
    }
}

#[test]
fn esc_clears_search_then_returns_back() {
    // First Esc clears a non-empty query (matching SelectPrompt's
    // searchable contract), and a subsequent Esc cancels the screen.
    let mut s = ListScreen::new(true, true);
    s.set_worktrees(worktrees());
    s.handle_key(key(KeyCode::Char('f')));
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, ListAction::Continue);
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, ListAction::Back);
}

#[test]
fn esc_returns_back() {
    let mut s = ListScreen::new(true, true);
    s.set_worktrees(worktrees());
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, ListAction::Back);
}

#[test]
fn enter_opens_action_menu_and_navigate_to_emits_path_when_from_wrapper() {
    let mut s = ListScreen::new(true, true);
    s.set_worktrees(worktrees());
    s.handle_key(key(KeyCode::Enter));
    // Now in action menu; first option is "Navigate to Directory" (enabled).
    let action = s.handle_key(key(KeyCode::Enter));
    match action {
        ListAction::NavigateTo(path) => assert_eq!(path, "/tmp/repo-feat"),
        other => panic!("expected NavigateTo, got {other:?}"),
    }
}

#[test]
fn navigate_to_disabled_outside_wrapper() {
    let mut s = ListScreen::new(false, true);
    s.set_worktrees(worktrees());
    s.handle_key(key(KeyCode::Enter));
    // First option is disabled; pressing Enter on a disabled item does not select.
    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, ListAction::Continue);
}

#[test]
fn open_with_command_in_action_menu_emits_open_terminal() {
    let mut s = ListScreen::new(true, true);
    s.set_worktrees(worktrees());
    s.handle_key(key(KeyCode::Enter)); // Enter action menu
    s.handle_key(key(KeyCode::Down)); // move to "Open with Command"
    let action = s.handle_key(key(KeyCode::Enter));
    match action {
        ListAction::OpenTerminal(path) => assert_eq!(path, "/tmp/repo-feat"),
        other => panic!("expected OpenTerminal, got {other:?}"),
    }
}

#[test]
fn esc_in_action_menu_returns_to_list() {
    let mut s = ListScreen::new(true, true);
    s.set_worktrees(worktrees());
    s.handle_key(key(KeyCode::Enter));
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, ListAction::Continue);
    // After cancel, pressing Esc returns Back (we're back in List mode).
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, ListAction::Back);
}

#[test]
fn render_uses_select_prompt_design_with_hint() {
    let mut s = ListScreen::new(true, true);
    s.set_worktrees(worktrees());
    let dumped = dump(80, 12, |f| s.render(f, f.area()));
    // Mirrors the Source-branch page design: SelectPrompt with title,
    // branded "➤" cursor, numbered rows, "Search:" filter row, and the
    // navigation hint.
    assert!(dumped.contains("Select a worktree:"));
    assert!(dumped.contains("➤"));
    assert!(dumped.contains("1. "));
    assert!(dumped.contains("2. "));
    assert!(dumped.contains("/tmp/repo-feat"));
    assert!(dumped.contains("feat"));
    assert!(dumped.contains("Search:"));
    assert!(dumped.contains("filter"));
    assert!(dumped.contains("Navigate"));
    assert!(dumped.contains("Back"));
}

#[test]
fn render_action_menu_shows_selected_header() {
    let mut s = ListScreen::new(true, true);
    s.set_worktrees(worktrees());
    s.handle_key(key(KeyCode::Enter));
    let dumped = dump(80, 10, |f| s.render(f, f.area()));
    assert!(dumped.contains("Selected"));
    assert!(dumped.contains("feat"));
}

#[test]
fn error_overlay_renders_and_clears_on_keypress() {
    let mut s = ListScreen::new(true, true);
    s.set_error("boom".into());
    let dumped = dump(60, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("boom"));
    assert!(dumped.contains("Press any key"));
    let action = s.handle_key(key(KeyCode::Char('x')));
    assert_eq!(action, ListAction::Back);
    assert!(s.error().is_none());
}

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use wisetree::git::types::{BranchStatus, GitWorktree};
use wisetree::messages::colors;
use wisetree::services::{CommitSummary, DashboardRow, PrState, PullRequest};
use wisetree::tui::screens::dashboard::{DashboardAction, DashboardScreen};

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

fn dump_lines<F>(width: u16, height: u16, draw: F) -> String
where
    F: FnOnce(&mut ratatui::Frame),
{
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(draw).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end_matches(' ')
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn row(path: &str, branch: &str, is_clean: bool) -> DashboardRow {
    DashboardRow {
        worktree: GitWorktree {
            path: path.into(),
            branch: branch.into(),
            commit: "deadbeef".into(),
            is_main: branch == "main",
            is_clean,
            branch_status: Some(BranchStatus {
                ahead: if branch == "bug" { 1 } else { 0 },
                behind: 0,
                upstream_branch: Some("origin/main".into()),
            }),
        },
        last_commit: Some(CommitSummary {
            sha: "deadbee".into(),
            summary: format!("work on {branch}"),
            relative_time: "2 minutes ago".into(),
            author: "Test".into(),
        }),
        pull_request: None,
        error: None,
    }
}

fn row_with_pr(path: &str, branch: &str, is_clean: bool) -> DashboardRow {
    let mut row = row(path, branch, is_clean);
    row.pull_request = Some(PullRequest {
        number: 42,
        state: PrState::Open,
        url: "https://github.com/example/repo/pull/42".into(),
        title: "Improve dashboard footer details for live workflows".into(),
    });
    row
}

fn row_with_pr_state(path: &str, branch: &str, is_clean: bool, state: PrState) -> DashboardRow {
    let mut row = row_with_pr(path, branch, is_clean);
    if let Some(pr) = row.pull_request.as_mut() {
        pr.state = state;
    }
    row
}

fn ready_screen(is_from_wrapper: bool) -> DashboardScreen {
    let mut screen = DashboardScreen::new(
        is_from_wrapper,
        true,
        true,
        vec![
            "branch".into(),
            "status".into(),
            "ahead_behind".into(),
            "last_commit".into(),
        ],
        Vec::new(),
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row("/tmp/repo-bug", "bug", false),
        row("/tmp/repo-feat", "feat", true),
    ]);
    screen
}

#[test]
fn loading_render_shows_loading_message() {
    let screen = DashboardScreen::new(true, true, true, vec!["branch".into()], Vec::new());
    let dumped = dump(80, 8, |f| screen.render(f, f.area()));
    assert!(dumped.contains("Loading dashboard"));
}

#[test]
fn empty_state_renders_no_worktrees_found() {
    let mut screen = DashboardScreen::new(true, true, true, vec!["branch".into()], Vec::new());
    screen.set_rows(vec![]);
    let dumped = dump(80, 8, |f| screen.render(f, f.area()));
    assert!(dumped.contains("No worktrees found"));
}

#[test]
fn table_renders_configured_columns_in_order() {
    let screen = ready_screen(true);
    let dumped = dump(120, 12, |f| screen.render(f, f.area()));
    let branch = dumped.find("Branch").unwrap();
    let status = dumped.find("Status").unwrap();
    let ahead = dumped.find("Ahead/Behind").unwrap();
    let last_commit = dumped.find("Last Commit").unwrap();
    assert!(branch < status && status < ahead && ahead < last_commit);
}

#[test]
fn search_filter_narrows_then_escape_clears_before_back() {
    let mut screen = ready_screen(true);
    // Always-on search: typing characters directly filters the list.
    screen.handle_key(key(KeyCode::Char('b')));
    screen.handle_key(key(KeyCode::Char('u')));
    screen.handle_key(key(KeyCode::Char('g')));

    let filtered = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(filtered.contains("repo-bug"));
    assert!(!filtered.contains("repo-feat"));

    assert_eq!(
        screen.handle_key(key(KeyCode::Esc)),
        DashboardAction::Continue
    );
    let cleared = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(cleared.contains("repo-feat"));
    assert_eq!(screen.handle_key(key(KeyCode::Esc)), DashboardAction::Back);
}

#[test]
fn search_matches_status_text() {
    let mut screen = ready_screen(true);
    for c in "dirty".chars() {
        screen.handle_key(key(KeyCode::Char(c)));
    }
    let filtered = dump(120, 12, |f| screen.render(f, f.area()));
    // Only repo-bug is dirty in ready_screen.
    assert!(filtered.contains("repo-bug"));
    assert!(!filtered.contains("repo-feat"));
}

#[test]
fn search_matches_ahead_behind_text() {
    let mut screen = ready_screen(true);
    // Only the "bug" branch is +1 ahead per the row() helper.
    for c in "+1".chars() {
        screen.handle_key(key(KeyCode::Char(c)));
    }
    let filtered = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(filtered.contains("repo-bug"));
    assert!(!filtered.contains("repo-feat"));
}

#[test]
fn backspace_removes_last_character_from_query() {
    let mut screen = ready_screen(true);
    // Type "bugxyz" — matches nothing.
    for c in "bugxyz".chars() {
        screen.handle_key(key(KeyCode::Char(c)));
    }
    let empty = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(empty.contains("No worktrees match"));

    // Backspace 3 times → query becomes "bug" → narrows to repo-bug only.
    screen.handle_key(key(KeyCode::Backspace));
    screen.handle_key(key(KeyCode::Backspace));
    screen.handle_key(key(KeyCode::Backspace));
    let narrowed = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(narrowed.contains("repo-bug"));
    assert!(!narrowed.contains("repo-feat"));
}

#[test]
fn ctrl_r_emits_refresh_action() {
    let mut screen = ready_screen(true);
    let ctrl_r = KeyEvent {
        code: KeyCode::Char('r'),
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    };
    assert_eq!(screen.handle_key(ctrl_r), DashboardAction::Refresh);
    // Plain 'r' should now be added to the search query, not refresh.
    assert_eq!(
        screen.handle_key(key(KeyCode::Char('r'))),
        DashboardAction::Continue
    );
}

#[test]
fn action_menu_only_shows_navigate_when_wrapper_mode_enabled() {
    let mut wrapper = ready_screen(true);
    wrapper.handle_key(key(KeyCode::Enter));
    let dumped = dump(100, 12, |f| wrapper.render(f, f.area()));
    assert!(dumped.contains("Navigate to Directory"));
    assert!(dumped.contains("Copy path to clipboard"));

    let mut plain = DashboardScreen::new(
        false,
        true,
        false,
        vec!["branch".into(), "status".into()],
        Vec::new(),
    );
    plain.set_rows(vec![row("/tmp/repo", "main", true)]);
    plain.handle_key(key(KeyCode::Enter));
    let dumped = dump(100, 12, |f| plain.render(f, f.area()));
    assert!(!dumped.contains("Navigate to Directory"));
    assert!(!dumped.contains("Copy path to clipboard"));
}

#[test]
fn jump_to_delete_action_is_emitted_for_selected_row() {
    // Without wrapper-mode the menu is Open / Delete / Copy, so a single
    // Down lands on Delete (Copy is intentionally below Delete now).
    let mut screen = ready_screen(false);
    screen.handle_key(key(KeyCode::Enter));
    let action = screen.handle_key(key(KeyCode::Down));
    assert_eq!(action, DashboardAction::Continue);
    match screen.handle_key(key(KeyCode::Enter)) {
        DashboardAction::JumpToDelete(path) => assert_eq!(path, "/tmp/repo"),
        other => panic!("expected JumpToDelete, got {other:?}"),
    }
}

#[test]
fn dirty_row_uses_error_palette() {
    let screen = ready_screen(true);
    let backend = TestBackend::new(120, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| screen.render(f, f.area())).unwrap();
    let buffer = terminal.backend().buffer();

    let dirty_cell = buffer
        .content
        .iter()
        .find(|cell| cell.symbol() == "D" && cell.fg == colors::ERROR)
        .expect("dirty cell with error color");
    assert_eq!(dirty_cell.fg, colors::ERROR);
}

#[test]
fn clean_row_uses_accent_palette() {
    let screen = ready_screen(true);
    let backend = TestBackend::new(120, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| screen.render(f, f.area())).unwrap();
    let buffer = terminal.backend().buffer();

    let clean_cell = buffer
        .content
        .iter()
        .find(|cell| cell.symbol() == "C" && cell.fg == colors::ACCENT)
        .expect("clean cell with accent color");
    assert_eq!(clean_cell.fg, colors::ACCENT);
}

#[test]
fn opened_pr_row_renders_opened_status_in_warning_palette() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
    );
    screen.set_rows(vec![row_with_pr_state(
        "/tmp/repo-bug",
        "bug",
        false,
        PrState::Open,
    )]);

    let backend = TestBackend::new(120, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| screen.render(f, f.area())).unwrap();
    let buffer = terminal.backend().buffer();

    let opened_cell = buffer
        .content
        .iter()
        .find(|cell| cell.symbol() == "O" && cell.fg == colors::WARNING)
        .expect("opened cell with warning color");
    assert_eq!(opened_cell.fg, colors::WARNING);

    let dumped = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(dumped.contains("Opened"));
}

#[test]
fn merged_pr_row_renders_merged_status_in_info_palette() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
    );
    screen.set_rows(vec![row_with_pr_state(
        "/tmp/repo-bug",
        "bug",
        true,
        PrState::Merged,
    )]);

    let backend = TestBackend::new(120, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| screen.render(f, f.area())).unwrap();
    let buffer = terminal.backend().buffer();

    let merged_cell = buffer
        .content
        .iter()
        .find(|cell| cell.symbol() == "M" && cell.fg == colors::INFO)
        .expect("merged cell with info color");
    assert_eq!(merged_cell.fg, colors::INFO);

    let dumped = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(dumped.contains("Merged"));
}

#[test]
fn search_matches_opened_status_text() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr_state("/tmp/repo-bug", "bug", false, PrState::Open),
    ]);
    for c in "opened".chars() {
        screen.handle_key(key(KeyCode::Char(c)));
    }
    let filtered = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(filtered.contains("repo-bug"));
    assert!(!filtered.contains("/tmp/repo "));
}

#[test]
fn overflow_rows_show_more_above_and_below_indicators() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
    );
    let rows: Vec<DashboardRow> = (0..12)
        .map(|index| {
            row(
                &format!("/tmp/repo-{index}"),
                &format!("feat-{index}"),
                true,
            )
        })
        .collect();
    screen.set_rows(rows);
    for _ in 0..11 {
        screen.handle_key(key(KeyCode::Down));
    }

    // Height must fit: 4 (banner/search) + 13 (header + 2 overflow + 10 rows)
    // + 4 (4-line footer with Status / Ahead-Behind legends).
    let dumped = dump(120, 21, |f| screen.render(f, f.area()));
    assert!(dumped.contains("more above"));
    assert!(dumped.contains("more below") || dumped.contains("bottom"));
}

#[test]
fn selected_row_warning_is_rendered_in_footer() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
    );
    let mut broken = row("/tmp/repo-bug", "bug", false);
    broken.error = Some("status timed out".into());
    screen.set_rows(vec![row("/tmp/repo", "main", true), broken]);
    screen.handle_key(key(KeyCode::Down));

    let dumped = dump(120, 14, |f| screen.render(f, f.area()));
    assert!(dumped.contains("Selected row warning"));
    assert!(dumped.contains("status timed out"));
}

#[test]
fn wide_render_snapshot_includes_pr_footer_detail() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec![
            "branch".into(),
            "status".into(),
            "ahead_behind".into(),
            "last_commit".into(),
            "pull_request".into(),
        ],
        Vec::new(),
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr("/tmp/repo-bug", "bug", false),
    ]);
    screen.handle_key(key(KeyCode::Down));

    insta::assert_snapshot!(
        "dashboard_wide_pr_footer",
        dump_lines(110, 14, |f| screen.render(f, f.area()))
    );
}

#[test]
fn narrow_render_snapshot_collapses_trailing_columns() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec![
            "branch".into(),
            "status".into(),
            "ahead_behind".into(),
            "last_commit".into(),
            "pull_request".into(),
        ],
        Vec::new(),
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr("/tmp/repo-bug", "bug", false),
        row("/tmp/repo-feat", "feat", true),
    ]);
    screen.handle_key(key(KeyCode::Down));

    insta::assert_snapshot!(
        "dashboard_narrow_collapsed_columns",
        dump_lines(72, 16, |f| screen.render(f, f.area()))
    );
}

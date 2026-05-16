use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use wisetree::git::types::{BranchStatus, GitWorktree};
use wisetree::messages::colors;
use wisetree::services::{
    CheckStatus, CommitSummary, DashboardNotice, DashboardNoticeLevel, DashboardRow, MergeStatus,
    PrState, PullRequest, ReviewStatus,
};
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
        checks_status: None,
        review_status: None,
        merge_status: None,
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

fn row_with_check(path: &str, branch: &str, is_clean: bool, checks: CheckStatus) -> DashboardRow {
    let mut row = row_with_pr(path, branch, is_clean);
    if let Some(pr) = row.pull_request.as_mut() {
        pr.checks_status = Some(checks);
    }
    row
}

fn row_with_review(path: &str, branch: &str, is_clean: bool, review: ReviewStatus) -> DashboardRow {
    let mut row = row_with_pr(path, branch, is_clean);
    if let Some(pr) = row.pull_request.as_mut() {
        pr.review_status = Some(review);
    }
    row
}

fn row_with_check_and_review(
    path: &str,
    branch: &str,
    is_clean: bool,
    checks: CheckStatus,
    review: ReviewStatus,
) -> DashboardRow {
    let mut row = row_with_pr(path, branch, is_clean);
    if let Some(pr) = row.pull_request.as_mut() {
        pr.checks_status = Some(checks);
        pr.review_status = Some(review);
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
        false,
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
    let mut screen =
        DashboardScreen::new(true, true, true, vec!["branch".into()], Vec::new(), false);
    let dumped = dump(80, 8, |f| screen.render(f, f.area()));
    assert!(dumped.contains("Loading dashboard"));
}

#[test]
fn empty_state_renders_no_worktrees_found() {
    let mut screen =
        DashboardScreen::new(true, true, true, vec!["branch".into()], Vec::new(), false);
    screen.set_rows(vec![]);
    let dumped = dump(80, 8, |f| screen.render(f, f.area()));
    assert!(dumped.contains("No worktrees found"));
}

#[test]
fn table_renders_configured_columns_in_order() {
    let mut screen = ready_screen(true);
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
        false,
    );
    plain.set_rows(vec![row("/tmp/repo", "main", true)]);
    plain.handle_key(key(KeyCode::Enter));
    let dumped = dump(100, 12, |f| plain.render(f, f.area()));
    assert!(!dumped.contains("Navigate to Directory"));
    assert!(!dumped.contains("Copy path to clipboard"));
}

#[test]
fn backspace_with_empty_search_jumps_to_delete_for_selected_row() {
    let mut screen = ready_screen(true);
    // First row is the mother worktree (protected) — move onto the
    // second row, which is a regular deletable worktree.
    screen.handle_key(key(KeyCode::Down));
    match screen.handle_key(key(KeyCode::Backspace)) {
        DashboardAction::JumpToDelete(path) => assert_eq!(path, "/tmp/repo-bug"),
        other => panic!("expected JumpToDelete, got {other:?}"),
    }
}

#[test]
fn backspace_on_mother_worktree_emits_protected_action() {
    let mut screen = ready_screen(true);
    // Default selection is the first row, which is the mother worktree.
    assert_eq!(
        screen.handle_key(key(KeyCode::Backspace)),
        DashboardAction::MotherWorktreeProtected
    );
}

#[test]
fn backspace_while_typing_only_edits_search_query() {
    let mut screen = ready_screen(true);
    for c in "bug".chars() {
        screen.handle_key(key(KeyCode::Char(c)));
    }
    // Backspace while the query is non-empty must edit it, not jump to delete.
    assert_eq!(
        screen.handle_key(key(KeyCode::Backspace)),
        DashboardAction::Continue
    );
    let dumped = dump(120, 12, |f| screen.render(f, f.area()));
    // Query is now "bu" — repo-bug still matches, repo-feat does not.
    assert!(dumped.contains("repo-bug"));
    assert!(!dumped.contains("repo-feat"));
}

#[test]
fn action_menu_no_longer_exposes_delete_choice() {
    let mut screen = ready_screen(true);
    screen.handle_key(key(KeyCode::Enter));
    let dumped = dump(120, 14, |f| screen.render(f, f.area()));
    assert!(!dumped.contains("Delete this worktree"));
}

#[test]
fn selected_worktree_row_shows_selection_marker() {
    let mut screen = ready_screen(true);
    screen.handle_key(key(KeyCode::Down));

    let dumped = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(dumped.contains(" ➤ /tmp/repo-bug"));
}

#[test]
fn dirty_row_uses_error_palette() {
    let mut screen = ready_screen(true);
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
    let mut screen = ready_screen(true);
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
fn opened_pr_row_renders_opened_status_in_info_palette() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr_state("/tmp/repo-bug", "bug", false, PrState::Open),
    ]);

    let backend = TestBackend::new(120, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| screen.render(f, f.area())).unwrap();
    let buffer = terminal.backend().buffer();

    let opened_cell = buffer
        .content
        .iter()
        .find(|cell| cell.symbol() == "O" && cell.fg == colors::INFO)
        .expect("opened cell with info color");
    assert_eq!(opened_cell.fg, colors::INFO);

    let dumped = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(dumped.contains("Opened"));
}

#[test]
fn merged_pr_row_renders_merged_status_in_success_palette() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr_state("/tmp/repo-bug", "bug", true, PrState::Merged),
    ]);

    let backend = TestBackend::new(120, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| screen.render(f, f.area())).unwrap();
    let buffer = terminal.backend().buffer();

    let merged_cell = buffer
        .content
        .iter()
        .find(|cell| cell.symbol() == "M" && cell.fg == colors::SUCCESS)
        .expect("merged cell with success color");
    assert_eq!(merged_cell.fg, colors::SUCCESS);

    let dumped = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(dumped.contains("Merged"));
}

#[test]
fn opened_pr_with_running_checks_renders_yellow_circle() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_check("/tmp/repo-bug", "bug", false, CheckStatus::Running),
    ]);
    let dumped = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(
        dumped.contains("Opened 🟡"),
        "expected `Opened 🟡` in rendered output: {dumped}"
    );
}

#[test]
fn opened_pr_with_pending_review_renders_raised_hand() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_review("/tmp/repo-bug", "bug", false, ReviewStatus::Pending),
    ]);
    let dumped = dump_lines(120, 14, |f| screen.render(f, f.area()));
    let opened_row = dumped
        .lines()
        .find(|line| line.contains("repo-bug") && line.contains("Opened"))
        .unwrap_or_else(|| panic!("missing row with `Opened` label: {dumped}"));
    assert!(
        opened_row.contains("Opened ✋"),
        "Status cell must include the pending-review hand: {opened_row}"
    );
}

#[test]
fn opened_pr_with_running_check_and_approved_review_renders_both_emojis() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_check_and_review(
            "/tmp/repo-bug",
            "bug",
            false,
            CheckStatus::Running,
            ReviewStatus::Approved,
        ),
    ]);
    let dumped = dump_lines(120, 14, |f| screen.render(f, f.area()));
    let opened_row = dumped
        .lines()
        .find(|line| line.contains("repo-bug") && line.contains("Opened"))
        .unwrap_or_else(|| panic!("missing row with `Opened` label: {dumped}"));
    // ratatui pads each 2-column emoji with a continuation cell, so the
    // grapheme dump shows an extra space between the two emojis.
    assert!(
        opened_row.contains("Opened 🟡") && opened_row.contains("👍"),
        "Status cell must include both drone and review emojis: {opened_row}"
    );
    let drone_pos = opened_row.find("🟡").unwrap();
    let review_pos = opened_row.find("👍").unwrap();
    assert!(
        drone_pos < review_pos,
        "Drone emoji must render to the LEFT of review emoji: {opened_row}"
    );
}

#[test]
fn opened_pr_with_passed_check_and_rejected_review_renders_thumbs_down() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_check_and_review(
            "/tmp/repo-bug",
            "bug",
            false,
            CheckStatus::Passed,
            ReviewStatus::Rejected,
        ),
    ]);
    let dumped = dump_lines(120, 14, |f| screen.render(f, f.area()));
    let opened_row = dumped
        .lines()
        .find(|line| line.contains("repo-bug") && line.contains("Opened"))
        .unwrap_or_else(|| panic!("missing row with `Opened` label: {dumped}"));
    assert!(
        opened_row.contains("Opened 🟢") && opened_row.contains("👎"),
        "Status cell must show drone + changes-requested thumb: {opened_row}"
    );
    let drone_pos = opened_row.find("🟢").unwrap();
    let review_pos = opened_row.find("👎").unwrap();
    assert!(
        drone_pos < review_pos,
        "Drone emoji must render to the LEFT of review emoji: {opened_row}"
    );
}

#[test]
fn merged_pr_with_review_status_still_renders_plain_label() {
    // Review emoji must only surface for Open PRs. Stale review data on a
    // merged PR (e.g. left over from before the merge) should never show
    // up in the Merged label.
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    let mut merged = row_with_pr_state("/tmp/repo-bug", "bug", true, PrState::Merged);
    if let Some(pr) = merged.pull_request.as_mut() {
        pr.review_status = Some(ReviewStatus::Approved);
    }
    screen.set_rows(vec![row("/tmp/repo", "main", true), merged]);
    let dumped = dump_lines(120, 14, |f| screen.render(f, f.area()));
    let merged_row = dumped
        .lines()
        .find(|line| line.contains("repo-bug") && line.contains("Merged"))
        .unwrap_or_else(|| panic!("missing row with `Merged` label: {dumped}"));
    for emoji in ["✋", "👍", "👎"] {
        assert!(
            !merged_row.contains(emoji),
            "Merged row must not surface review emoji {emoji}: {merged_row}"
        );
    }
}

#[test]
fn opened_pr_with_failed_checks_renders_red_circle() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_check("/tmp/repo-bug", "bug", false, CheckStatus::Failed),
    ]);
    let dumped = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(dumped.contains("Opened 🔴"));
}

#[test]
fn opened_pr_without_checks_keeps_plain_label() {
    // Regression: PRs from repos without CI configured (or before any
    // checks have reported) must still render the historical "Opened"
    // label, NOT a circle. This protects backwards visual compatibility.
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr_state("/tmp/repo-bug", "bug", false, PrState::Open),
    ]);
    let dumped = dump_lines(120, 14, |f| screen.render(f, f.area()));
    let opened_row = dumped
        .lines()
        .find(|line| line.contains("repo-bug") && line.contains("Opened"))
        .unwrap_or_else(|| panic!("missing row with `Opened` label: {dumped}"));
    for emoji in ["⚪", "🟡", "🟢", "🔴", "⚠"] {
        assert!(
            !opened_row.contains(emoji),
            "Status cell must not contain {emoji} for an Opened PR with no checks: {opened_row}"
        );
    }
}

#[test]
fn search_matches_opened_for_rows_with_check_circles() {
    // Searching for "opened" must still match rows that render
    // "Opened 🟢" — the filter compares against the base label, not
    // the rendered text with the emoji suffix.
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_check("/tmp/repo-bug", "bug", false, CheckStatus::Passed),
    ]);
    for c in "opened".chars() {
        screen.handle_key(key(KeyCode::Char(c)));
    }
    let filtered = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(filtered.contains("repo-bug"));
}

#[test]
fn search_matches_opened_status_text() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
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
fn table_uses_available_height_before_scrolling() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
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

    // Height must fit exactly: 4 (banner/search) + 13 (header + 12 rows)
    // + 9 (9-line footer with bordered bulk-delete buttons row + checks/reviews legends).
    let dumped = dump(120, 26, |f| screen.render(f, f.area()));
    assert!(dumped.contains("repo-11"));
    assert!(!dumped.contains("more above"));
    assert!(!dumped.contains("more below"));
}

#[test]
fn overflow_rows_show_more_above_and_below_indicators() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    let rows: Vec<DashboardRow> = (0..15)
        .map(|index| {
            row(
                &format!("/tmp/repo-{index}"),
                &format!("feat-{index}"),
                true,
            )
        })
        .collect();
    screen.set_rows(rows);
    for _ in 0..7 {
        screen.handle_key(key(KeyCode::Down));
    }

    // Height must fit: 4 (banner/search) + 13 (header + 2 overflow + 10 rows)
    // + 9 (9-line footer with Status / Checks / Reviews / Ahead-Behind legends).
    let dumped = dump(120, 26, |f| screen.render(f, f.area()));
    assert!(dumped.contains("more above"));
    assert!(dumped.contains("more below"));
}

#[test]
fn footer_includes_checks_legend_with_all_circles() {
    let mut screen = ready_screen(true);
    let dumped = dump(140, 22, |f| screen.render(f, f.area()));
    assert!(
        dumped.contains("Checks:"),
        "expected Checks legend: {dumped}"
    );
    for label in ["Pending", "Running", "Passed", "Failed", "Errored"] {
        assert!(
            dumped.contains(label),
            "missing {label} in legend: {dumped}"
        );
    }
    for emoji in ["⚪", "🟡", "🟢", "🔴", "⚠"] {
        assert!(
            dumped.contains(emoji),
            "missing {emoji} in legend: {dumped}"
        );
    }
}

#[test]
fn footer_includes_reviews_legend_below_checks_legend() {
    let mut screen = ready_screen(true);
    let dumped = dump_lines(140, 22, |f| screen.render(f, f.area()));

    let reviews_line = dumped
        .lines()
        .find(|line| line.contains("PR Reviews:"))
        .unwrap_or_else(|| panic!("missing PR Reviews legend: {dumped}"));
    for emoji in ["✋", "👍", "👎"] {
        assert!(
            reviews_line.contains(emoji),
            "missing {emoji} in PR Reviews legend: {reviews_line}"
        );
    }
    for label in ["Pending", "Approved", "Changes Requested"] {
        assert!(
            reviews_line.contains(label),
            "missing {label} in PR Reviews legend: {reviews_line}"
        );
    }

    let checks_line_no = dumped
        .lines()
        .position(|line| line.contains("PR Checks:"))
        .unwrap();
    let reviews_line_no = dumped
        .lines()
        .position(|line| line.contains("PR Reviews:"))
        .unwrap();
    assert!(
        checks_line_no < reviews_line_no,
        "PR Reviews legend must render below PR Checks legend"
    );
}

#[test]
fn selected_row_warning_is_rendered_in_footer() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
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
fn dashboard_notice_renders_in_footer_detail_slot() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr("/tmp/repo-bug", "bug", false),
    ]);
    screen.handle_key(key(KeyCode::Down));
    screen.set_notice(DashboardNotice {
        level: DashboardNoticeLevel::Error,
        message: "GitHub PR refresh failed: auth error - showing cached data.".into(),
    });

    let dumped = dump_lines(110, 16, |f| screen.render(f, f.area()));
    assert!(dumped.contains("GitHub PR refresh failed: auth error - showing cached data."));
    assert!(dumped.contains("Delete worktrees with status:"));
    assert!(!dumped.contains("PR #42 Open"));
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
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr("/tmp/repo-bug", "bug", false),
    ]);
    screen.handle_key(key(KeyCode::Down));

    insta::assert_snapshot!(
        "dashboard_wide_pr_footer",
        dump_lines(110, 18, |f| screen.render(f, f.area()))
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
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr("/tmp/repo-bug", "bug", false),
        row("/tmp/repo-feat", "feat", true),
    ]);
    screen.handle_key(key(KeyCode::Down));

    insta::assert_snapshot!(
        "dashboard_narrow_collapsed_columns",
        dump_lines(72, 18, |f| screen.render(f, f.area()))
    );
}

fn open_action_menu_for_second_row(screen: &mut DashboardScreen) -> String {
    screen.handle_key(key(KeyCode::Down));
    screen.handle_key(key(KeyCode::Enter));
    dump(120, 14, |f| screen.render(f, f.area()))
}

#[test]
fn action_menu_shows_merge_option_for_open_pr_row() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr_state("/tmp/repo-bug", "bug", false, PrState::Open),
    ]);

    let dumped = open_action_menu_for_second_row(&mut screen);
    assert!(
        dumped.contains("Merge Pull Request"),
        "expected `Merge Pull Request` for an Open PR row: {dumped}"
    );
    assert!(dumped.contains("Open Pull Request"));
}

#[test]
fn action_menu_hides_merge_option_for_merged_pr_row() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr_state("/tmp/repo-bug", "bug", true, PrState::Merged),
    ]);

    let dumped = open_action_menu_for_second_row(&mut screen);
    assert!(
        !dumped.contains("Merge Pull Request"),
        "Merge Pull Request must not appear for a Merged PR row: {dumped}"
    );
    // Already-merged PRs should still expose the `Open Pull Request` link.
    assert!(dumped.contains("Open Pull Request"));
}

#[test]
fn action_menu_hides_merge_option_for_closed_pr_row() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr_state("/tmp/repo-bug", "bug", false, PrState::Closed),
    ]);

    let dumped = open_action_menu_for_second_row(&mut screen);
    assert!(!dumped.contains("Merge Pull Request"));
}

#[test]
fn action_menu_hides_merge_option_for_draft_pr_row() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr_state("/tmp/repo-bug", "bug", false, PrState::Draft),
    ]);

    let dumped = open_action_menu_for_second_row(&mut screen);
    assert!(!dumped.contains("Merge Pull Request"));
}

#[test]
fn action_menu_hides_merge_option_when_no_pr_present() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row("/tmp/repo-bug", "bug", false),
    ]);

    let dumped = open_action_menu_for_second_row(&mut screen);
    assert!(!dumped.contains("Merge Pull Request"));
    assert!(!dumped.contains("Open Pull Request"));
}

// ---- "Update Pull Request" visibility ----------------------------------
// The option lives in the action menu only when the row carries an Open
// PR *and* is behind its base — either `merge_status == Behind` or
// `branch_status.behind > 0`. These tests pin both the positive and the
// negative cases so the gate can't silently drift.

fn behind_row_via_branch_status(path: &str, branch: &str) -> DashboardRow {
    let mut row = row_with_pr_state(path, branch, true, PrState::Open);
    row.worktree.branch_status = Some(BranchStatus {
        ahead: 1,
        behind: 3,
        upstream_branch: Some("upstream/main".into()),
    });
    row
}

fn behind_row_via_merge_status(path: &str, branch: &str) -> DashboardRow {
    let mut row = row_with_pr_state(path, branch, true, PrState::Open);
    if let Some(pr) = row.pull_request.as_mut() {
        pr.merge_status = Some(MergeStatus::Behind);
    }
    row
}

#[test]
fn action_menu_shows_update_option_for_open_pr_behind_via_branch_status() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        behind_row_via_branch_status("/tmp/repo-bug", "bug"),
    ]);

    let dumped = open_action_menu_for_second_row(&mut screen);
    assert!(
        dumped.contains("Update Pull Request"),
        "expected `Update Pull Request` for Open+behind row: {dumped}"
    );
}

#[test]
fn action_menu_shows_update_option_for_open_pr_behind_via_merge_status() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        behind_row_via_merge_status("/tmp/repo-bug", "bug"),
    ]);

    let dumped = open_action_menu_for_second_row(&mut screen);
    assert!(
        dumped.contains("Update Pull Request"),
        "expected `Update Pull Request` when merge_status==Behind: {dumped}"
    );
}

#[test]
fn action_menu_hides_update_option_for_open_pr_when_up_to_date() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        // `row_with_pr_state` keeps merge_status=None and behind=0 — so the
        // row is Open but not behind its base.
        row_with_pr_state("/tmp/repo-bug", "bug", true, PrState::Open),
    ]);

    let dumped = open_action_menu_for_second_row(&mut screen);
    assert!(
        !dumped.contains("Update Pull Request"),
        "Update Pull Request must hide when branch is up to date: {dumped}"
    );
    // Merge Pull Request must still appear (Open and up to date is fine to merge).
    assert!(dumped.contains("Merge Pull Request"));
}

#[test]
fn action_menu_hides_update_option_for_merged_pr_even_if_behind() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    let mut behind_merged = behind_row_via_branch_status("/tmp/repo-bug", "bug");
    if let Some(pr) = behind_merged.pull_request.as_mut() {
        pr.state = PrState::Merged;
    }
    screen.set_rows(vec![row("/tmp/repo", "main", true), behind_merged]);

    let dumped = open_action_menu_for_second_row(&mut screen);
    assert!(!dumped.contains("Update Pull Request"));
}

#[test]
fn action_menu_hides_update_option_for_closed_pr() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    let mut behind_closed = behind_row_via_branch_status("/tmp/repo-bug", "bug");
    if let Some(pr) = behind_closed.pull_request.as_mut() {
        pr.state = PrState::Closed;
    }
    screen.set_rows(vec![row("/tmp/repo", "main", true), behind_closed]);

    let dumped = open_action_menu_for_second_row(&mut screen);
    assert!(!dumped.contains("Update Pull Request"));
}

#[test]
fn action_menu_hides_update_option_for_draft_pr() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    let mut behind_draft = behind_row_via_branch_status("/tmp/repo-bug", "bug");
    if let Some(pr) = behind_draft.pull_request.as_mut() {
        pr.state = PrState::Draft;
    }
    screen.set_rows(vec![row("/tmp/repo", "main", true), behind_draft]);

    let dumped = open_action_menu_for_second_row(&mut screen);
    assert!(!dumped.contains("Update Pull Request"));
}

#[test]
fn action_menu_hides_update_option_when_no_pr_present() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    let mut behind_no_pr = row("/tmp/repo-bug", "bug", true);
    behind_no_pr.worktree.branch_status = Some(BranchStatus {
        ahead: 0,
        behind: 5,
        upstream_branch: Some("upstream/main".into()),
    });
    screen.set_rows(vec![row("/tmp/repo", "main", true), behind_no_pr]);

    let dumped = open_action_menu_for_second_row(&mut screen);
    assert!(!dumped.contains("Update Pull Request"));
}

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::layout::Position;
use ratatui::Terminal;

use wisetree::git::types::{BranchStatus, GitWorktree};
use wisetree::messages::colors;

use wisetree::services::{
    CheckStatus, CommitSummary, DashboardNotice, DashboardNoticeLevel, DashboardRow, PrState,
    PullRequest, ReviewStatus, ReviewerSummary,
};
use wisetree::tui::screens::dashboard::{BulkDeleteStatus, DashboardAction, DashboardScreen};

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
        repository: None,
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
                source_branch: None,
                insertions: None,
                deletions: None,
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
        head_repository: None,
        number: 42,
        state: PrState::Open,
        url: "https://github.com/example/repo/pull/42".into(),
        title: "Improve dashboard footer details for live workflows".into(),
        base_ref_name: Some("main".into()),
        base_repository: Some("example/repo".into()),
        head_ref_oid: Some("abc123".into()),
        labels: vec![],
        checks_status: None,
        review_status: None,
        merge_status: None,
        reviewers: Default::default(),
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
fn ctrl_backspace_with_empty_search_jumps_to_delete_for_selected_row() {
    let mut screen = ready_screen(true);
    // First row is the mother worktree (protected) — move onto the
    // second row, which is a regular deletable worktree.
    screen.handle_key(key(KeyCode::Down));
    match screen.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)) {
        DashboardAction::JumpToDelete(path) => assert_eq!(path, "/tmp/repo-bug"),
        other => panic!("expected JumpToDelete, got {other:?}"),
    }
}

#[test]
fn ctrl_backspace_on_mother_worktree_emits_protected_action() {
    let mut screen = ready_screen(true);
    // Default selection is the first row, which is the mother worktree.
    assert_eq!(
        screen.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)),
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
fn action_menu_exposes_delete_worktree_choice() {
    let mut screen = ready_screen(true);
    screen.handle_key(key(KeyCode::Down));
    screen.handle_key(key(KeyCode::Enter));
    assert!(screen
        .general_command_labels()
        .contains(&"Delete worktree".to_string()));
}

#[test]
fn selected_worktree_row_shows_selection_marker() {
    let mut screen = ready_screen(true);
    screen.handle_key(key(KeyCode::Down));

    let dumped = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(dumped.contains(" ➤ repo-bug"));
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
fn draft_pr_row_renders_drafted_status_in_gray_medium_palette() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    // A draft PR must no longer masquerade as "Clean": even on a clean
    // worktree it renders its own "Drafted" label in the medium gray —
    // distinct from the lighter gray "Closed" uses.
    screen.set_rows(vec![
        row("/tmp/repo", "main", true),
        row_with_pr_state("/tmp/repo-bug", "bug", true, PrState::Draft),
    ]);

    let backend = TestBackend::new(120, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| screen.render(f, f.area())).unwrap();
    let buffer = terminal.backend().buffer();

    let drafted_cell = buffer
        .content
        .iter()
        .find(|cell| cell.symbol() == "D" && cell.fg == colors::GRAY_MEDIUM)
        .expect("drafted cell with gray-medium color");
    assert_eq!(drafted_cell.fg, colors::GRAY_MEDIUM);
    assert_ne!(
        drafted_cell.fg,
        colors::GRAY_LIGHT,
        "Drafted must not reuse the Closed gray"
    );

    let dumped = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(dumped.contains("Drafted"));
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
fn opened_pr_with_reviewers_renders_each_group_in_footer() {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        Vec::new(),
        false,
    );
    let mut bug = row_with_pr_state("/tmp/repo-bug", "bug", false, PrState::Open);
    if let Some(pr) = bug.pull_request.as_mut() {
        pr.reviewers = ReviewerSummary {
            pending: vec!["dan".into()],
            approved: vec!["alice".into()],
            changes_requested: vec!["bob".into()],
            commented: vec!["carol".into()],
        };
    }
    screen.set_rows(vec![row("/tmp/repo", "main", true), bug]);
    screen.handle_key(key(KeyCode::Down));

    let dumped = dump_lines(140, 20, |f| screen.render(f, f.area()));
    let reviewers_line = dumped
        .lines()
        .find(|line| line.starts_with("Reviewers:"))
        .unwrap_or_else(|| panic!("reviewers line missing from footer: {dumped}"));
    for snippet in [
        "Pending @dan",
        "Approved @alice",
        "Rejected @bob",
        "Commented @carol",
    ] {
        assert!(
            reviewers_line.contains(snippet),
            "expected `{snippet}` in reviewers line: {reviewers_line}"
        );
    }

    let pr_line_no = dumped
        .lines()
        .position(|line| line.contains("PR #42 Open"))
        .expect("PR info line missing");
    let reviewers_line_no = dumped
        .lines()
        .position(|line| line.starts_with("Reviewers:"))
        .unwrap();
    assert!(
        pr_line_no < reviewers_line_no,
        "Reviewers line must appear below PR # info: pr={pr_line_no} reviewers={reviewers_line_no}"
    );
}

#[test]
fn merged_pr_with_reviewers_does_not_render_reviewers_line() {
    // Reviewers info is only meaningful while the PR is Open; for merged or
    // closed PRs the data is stale, so we suppress the line entirely.
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
        pr.reviewers = ReviewerSummary {
            approved: vec!["alice".into()],
            ..Default::default()
        };
    }
    screen.set_rows(vec![row("/tmp/repo", "main", true), merged]);
    screen.handle_key(key(KeyCode::Down));

    let dumped = dump_lines(140, 20, |f| screen.render(f, f.area()));
    assert!(
        !dumped.contains("Reviewers:"),
        "Merged PR should suppress the reviewers footer line: {dumped}"
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
    // + 12 (12-line footer with bordered bulk-delete buttons row, all legends
    // including PR merge status).
    let dumped = dump(120, 29, |f| screen.render(f, f.area()));
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
        dump_lines(110, 20, |f| screen.render(f, f.area()))
    );
}

/// `row` cut from `source` and measured against `base` — the PR's base once
/// its PR is cached, else `source` itself.
fn cut_from(mut row: DashboardRow, source: &str, base: &str) -> DashboardRow {
    let status = row.worktree.branch_status.as_mut().expect("branch status");
    status.source_branch = Some(source.into());
    status.upstream_branch = Some(base.into());
    row
}

fn from_screen(rows: Vec<DashboardRow>) -> DashboardScreen {
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "from".into(), "status".into()],
        Vec::new(),
        false,
    );
    screen.set_rows(rows);
    screen
}

/// The From cell's foreground on the line showing `worktree`.
fn from_cell_fg(
    buffer: &ratatui::buffer::Buffer,
    worktree: &str,
    from: &str,
) -> ratatui::style::Color {
    let line = |y| {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
    };
    let y = (0..buffer.area.height)
        .find(|y| line(*y).contains(worktree))
        .expect("worktree row");
    let x = (0..buffer.area.width)
        .find(|x| {
            line(y)
                .chars()
                .skip(usize::from(*x))
                .collect::<String>()
                .starts_with(from)
        })
        .expect("From cell");
    buffer[(x, y)].fg
}

#[test]
fn from_column_shows_the_branch_each_worktree_was_cut_from() {
    let mut screen = from_screen(vec![
        row("/tmp/repo", "main", true),
        cut_from(
            row("/tmp/repo-api", "api", true),
            "upstream/main",
            "upstream/main",
        ),
        cut_from(
            row("/tmp/repo-v137", "v137", true),
            "kaefer/release-v1.37",
            "kaefer/release-v1.37",
        ),
    ]);

    let dumped = dump_lines(120, 14, |f| screen.render(f, f.area()));
    let lines: Vec<&str> = dumped.lines().collect();
    let header = lines
        .iter()
        .find(|line| line.contains("Worktree"))
        .expect("header");
    let column = |title: &str| header.find(title).expect(title);
    assert!(
        column("Branch") < column("From") && column("From") < column("Status"),
        "From sits between Branch and Status: {header}"
    );
    let cell = |worktree: &str| {
        let line = lines
            .iter()
            .find(|line| line.ends_with(worktree) || line.contains(&format!("{worktree} ")))
            .expect(worktree);
        line.chars().skip(column("From")).collect::<String>()
    };
    assert!(cell("repo-api").starts_with("upstream/main"));
    assert!(cell("repo-v137").starts_with("kaefer/release-v1.37"));
    // The main checkout was not cut from any branch.
    assert!(cell("repo").starts_with("- "));
}

#[test]
fn pr_targeting_another_base_than_its_from_branch_is_flagged() {
    let mut screen = from_screen(vec![
        row("/tmp/repo", "main", true),
        cut_from(
            row_with_pr("/tmp/repo-bug", "bug", false),
            "kaefer/main",
            "kaefer/release-v1.37",
        ),
    ]);
    screen.handle_key(key(KeyCode::Down));

    let backend = TestBackend::new(110, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| screen.render(f, f.area())).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let dumped: String = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                + "\n"
        })
        .collect();

    // The PR title gives way so the whole warning fits on the footer line.
    let footer = dumped
        .lines()
        .find(|line| line.contains("PR #42 Open"))
        .expect("PR footer detail");
    assert!(
        footer.trim_end().ends_with(
            "PR compares against kaefer/release-v1.37 ≠ Branch created from kaefer/main"
        ),
        "{footer}"
    );
    assert!(footer.contains("PR #42 Open: Improve"), "{footer}");
    assert_eq!(
        from_cell_fg(&buffer, "repo-bug", "kaefer/main"),
        colors::WARNING
    );
}

#[test]
fn pr_targeting_its_from_branch_shows_no_base_warning() {
    let mut screen = from_screen(vec![
        row("/tmp/repo", "main", true),
        cut_from(
            row_with_pr("/tmp/repo-bug", "bug", false),
            "kaefer/main",
            "kaefer/main",
        ),
    ]);
    screen.handle_key(key(KeyCode::Down));

    let backend = TestBackend::new(110, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| screen.render(f, f.area())).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let dumped: String = buffer.content.iter().map(|cell| cell.symbol()).collect();

    assert!(dumped.contains("PR #42 Open"));
    assert!(!dumped.contains("PR compares against"));
    assert_ne!(
        from_cell_fg(&buffer, "repo-bug", "kaefer/main"),
        colors::WARNING
    );
}

#[test]
fn search_matches_from_column_text() {
    let mut screen = from_screen(vec![
        row("/tmp/repo", "main", true),
        cut_from(
            row("/tmp/repo-api", "api", true),
            "upstream/main",
            "upstream/main",
        ),
        cut_from(
            row("/tmp/repo-v137", "v137", true),
            "kaefer/release-v1.37",
            "kaefer/release-v1.37",
        ),
    ]);
    for c in "kaefer".chars() {
        screen.handle_key(key(KeyCode::Char(c)));
    }

    let filtered = dump(120, 12, |f| screen.render(f, f.area()));
    assert!(filtered.contains("repo-v137"));
    assert!(!filtered.contains("repo-api"));
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
        dump_lines(72, 20, |f| screen.render(f, f.area()))
    );
}

fn open_action_menu_for_second_row(screen: &mut DashboardScreen) -> Vec<String> {
    screen.handle_key(key(KeyCode::Down));
    screen.handle_key(key(KeyCode::Enter));
    // Render once so the action menu (and its PR command buttons) is laid
    // out, then read the button labels straight from the screen rather than
    // scraping the rendered text — the short labels (Open/Merge/…) would
    // otherwise collide with general entries like "Open with Command".
    let _ = dump(120, 14, |f| screen.render(f, f.area()));
    screen.pr_command_labels()
}

#[test]
fn action_menu_renders_general_and_pull_request_sections() {
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

    screen.handle_key(key(KeyCode::Down));
    screen.handle_key(key(KeyCode::Enter));
    let dumped = dump(120, 16, |f| screen.render(f, f.area()));

    assert!(
        dumped.contains("General Commands"),
        "expected the General Commands heading: {dumped}"
    );
    assert!(
        dumped.contains("Pull Request Commands"),
        "expected the Pull Request Commands heading: {dumped}"
    );
    // The PR command buttons render their short labels.
    assert!(
        dumped.contains("Merge"),
        "expected a Merge button: {dumped}"
    );
    assert!(
        dumped.contains("Close"),
        "expected a Close button: {dumped}"
    );
}

#[test]
fn delete_worktree_action_jumps_to_delete_for_selected_row() {
    let mut screen = ready_screen(true);
    screen.handle_key(key(KeyCode::Down));
    screen.handle_key(key(KeyCode::Enter));
    for character in "Delete worktree".chars() {
        screen.handle_key(key(KeyCode::Char(character)));
    }

    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        DashboardAction::JumpToDelete("/tmp/repo-bug".into())
    );
}

#[test]
fn action_menu_hides_delete_worktree_for_main_row() {
    let mut screen = ready_screen(true);
    screen.handle_key(key(KeyCode::Enter));

    assert!(!screen
        .general_command_labels()
        .contains(&"Delete worktree".to_string()));
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

    let labels = open_action_menu_for_second_row(&mut screen);
    assert!(
        labels.iter().any(|l| l == "Merge"),
        "expected `Merge` button for an Open PR row: {labels:?}"
    );
    assert!(labels.iter().any(|l| l == "Open"));
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

    let labels = open_action_menu_for_second_row(&mut screen);
    assert!(
        !labels.iter().any(|l| l == "Merge"),
        "Merge must not appear for a Merged PR row: {labels:?}"
    );
    // Already-merged PRs should still expose the `Open` button.
    assert!(labels.iter().any(|l| l == "Open"));
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

    let labels = open_action_menu_for_second_row(&mut screen);
    assert!(!labels.iter().any(|l| l == "Merge"));
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

    let labels = open_action_menu_for_second_row(&mut screen);
    assert!(!labels.iter().any(|l| l == "Merge"));
}

#[test]
fn action_menu_shows_pr_commands_for_draft_pr_row() {
    // Draft PRs allow Open and Close; Merge requires an open PR.
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

    let labels = open_action_menu_for_second_row(&mut screen);
    assert!(
        labels.iter().any(|l| l == "Open"),
        "draft PR should expose Open: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "Close"),
        "draft PR should expose Close: {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l == "Merge"),
        "draft PR must not expose Merge: {labels:?}"
    );
}

#[test]
fn bulk_delete_drafted_targets_only_draft_pr_rows() {
    // The "Drafted" bulk-delete button must collect exactly the draft-PR
    // worktrees (never the mother, an open PR, or anything else).
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
        row_with_pr_state("/tmp/repo-draft", "draft", true, PrState::Draft),
        row_with_pr_state("/tmp/repo-open", "open", false, PrState::Open),
    ]);

    // Tab focuses bulk deletion; Right selects Closed, then Drafted.
    screen.handle_key(key(KeyCode::Tab));
    screen.handle_key(key(KeyCode::Right));
    screen.handle_key(key(KeyCode::Right));
    let action = screen.handle_key(key(KeyCode::Enter));

    match action {
        DashboardAction::BulkDelete(status, paths) => {
            assert_eq!(status, BulkDeleteStatus::Drafted);
            assert_eq!(paths, vec!["/tmp/repo-draft".to_string()]);
        }
        other => panic!("expected BulkDelete(Drafted, …), got {other:?}"),
    }
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

    let labels = open_action_menu_for_second_row(&mut screen);
    assert!(!labels.iter().any(|l| l == "Merge"));
    assert!(!labels.iter().any(|l| l == "Open"));
}

// Global scope reuses the same screen and existing row/action payloads.
fn global_row(mut row: DashboardRow, owner: &str, name: &str, local: bool) -> DashboardRow {
    row.repository = Some(wisetree::services::DashboardRepository {
        owner: owner.into(),
        name: name.into(),
        local_root: local.then(|| std::path::PathBuf::from(format!("/tmp/{owner}/{name}"))),
    });
    if !local {
        row.worktree.path.clear();
        row.worktree.branch_status = None;
        row.last_commit = None;
    }
    row
}

fn global_screen(rows: Vec<DashboardRow>) -> DashboardScreen {
    use wisetree::tui::screens::dashboard::DashboardScope;
    let mut screen = DashboardScreen::new(
        true,
        true,
        true,
        vec![
            "branch",
            "status",
            "ahead_behind",
            "diff",
            "last_commit",
            "pull_request",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        Vec::new(),
        true,
    )
    .with_scope(DashboardScope::Global);
    screen.set_rows(rows);
    screen
}

#[test]
fn global_columns_precede_configured_columns_and_survive_narrow_layouts() {
    let mut screen = global_screen(vec![global_row(
        row("/tmp/alpha/api/bug", "bug", false),
        "alpha",
        "api",
        true,
    )]);
    for width in [240, 72, 50] {
        let rendered = dump_lines(width, 24, |f| screen.render(f, f.area()));
        let header = rendered
            .lines()
            .find(|line| line.contains("Worktree"))
            .unwrap();
        assert!(
            header.find("Profile").unwrap() < header.find("Repository").unwrap(),
            "{header}"
        );
        if width == 240 {
            assert!(header.find("Repository").unwrap() < header.find("Branch").unwrap());
            assert!(header.contains("Last Commit"));
        } else {
            assert!(!header.contains("Last Commit"));
        }
        let data = rendered
            .lines()
            .find(|line| line.contains("alpha") && line.contains("api"))
            .unwrap();
        assert!(data.contains("bug"), "{data}");
    }
}

#[test]
fn global_search_matches_repository_identity_with_every_word_fuzzy() {
    for query in ["alpha", "api", "alpha/api", "alh/ap bug", "alpha dirty"] {
        let mut screen = global_screen(vec![
            global_row(row("/tmp/one/bug", "bug", false), "alpha", "api", true),
            global_row(row("/tmp/two/bug", "bug", false), "beta", "web", true),
        ]);
        for c in query.chars() {
            screen.handle_scoped_key(key(KeyCode::Char(c)));
        }
        let rendered = dump_lines(160, 24, |f| screen.render(f, f.area()));
        assert!(
            rendered
                .lines()
                .any(|line| line.contains("alpha") && line.contains("api")),
            "{query}: {rendered}"
        );
        assert!(!rendered.contains("beta"), "{query}: {rendered}");
    }
    let mut screen = global_screen(vec![global_row(
        row("/tmp/one/bug", "bug", false),
        "alpha",
        "api",
        true,
    )]);
    for c in "alpha nonexistent".chars() {
        screen.handle_scoped_key(key(KeyCode::Char(c)));
    }
    assert!(dump(160, 24, |f| screen.render(f, f.area()))
        .contains(wisetree::messages::GLOBAL_DASHBOARD_NO_MATCHES));
}

#[test]
fn remote_row_renders_pr_metadata_and_unavailable_local_columns() {
    use wisetree::services::MergeStatus;
    let mut remote = global_row(row_with_pr("", "feature-api", true), "alpha", "api", false);
    // Ignore local-looking data even if a stale row happens to carry it.
    remote.worktree.branch_status = row("/tmp/local", "local", true).worktree.branch_status;
    remote.last_commit = row("/tmp/local", "local", true).last_commit;
    let pr = remote.pull_request.as_mut().unwrap();
    pr.checks_status = Some(CheckStatus::Passed);
    pr.review_status = Some(ReviewStatus::Approved);
    pr.merge_status = Some(MergeStatus::Clean);
    let mut screen = global_screen(vec![remote]);
    let rendered = dump_lines(240, 24, |f| screen.render(f, f.area()));
    let header = rendered
        .lines()
        .find(|line| line.contains("Worktree"))
        .unwrap();
    let data = rendered
        .lines()
        .find(|line| line.contains("Not cloned locally"))
        .unwrap();
    for text in [
        "alpha",
        "api",
        "feature-api",
        "Opened",
        "🟢",
        "👍",
        "✅",
        "#42 Open",
    ] {
        assert!(data.contains(text), "missing {text}: {data}");
    }
    for title in ["Ahead/Behind", "Diff", "Last Commit"] {
        let start = header.find(title).unwrap();
        // All characters preceding these cells are ASCII in the header;
        // the backend includes continuation spaces for emoji in the data.
        assert_eq!(data.chars().nth(start), Some('-'), "{title}: {data}");
    }
    assert!(!data.contains("=0"));
    assert!(rendered.contains("1 worktrees, 0 dirty, 1 PR open"));
}

#[test]
fn remote_action_menu_only_offers_open_and_focuses_it_immediately() {
    use wisetree::tui::screens::dashboard::ScopedDashboardAction;
    for state in [
        PrState::Open,
        PrState::Draft,
        PrState::Merged,
        PrState::Closed,
    ] {
        let mut screen = global_screen(vec![global_row(
            row_with_pr_state("", "bug", true, state),
            "alpha",
            "api",
            false,
        )]);
        screen.handle_scoped_key(key(KeyCode::Enter));
        assert!(screen.general_command_labels().is_empty());
        assert_eq!(screen.pr_command_labels(), vec!["Open"]);
        let rendered = dump(120, 20, |f| screen.render(f, f.area()));
        assert!(!rendered.contains("General Commands"));
        assert!(rendered.contains("Not cloned locally"));
        for shortcut in ['m', 'c', 'd', 'x'] {
            assert_eq!(
                screen.handle_scoped_key(key(KeyCode::Char(shortcut))),
                ScopedDashboardAction::Dashboard(DashboardAction::Continue)
            );
        }
        screen.handle_scoped_key(key(KeyCode::Tab));
        match screen.handle_scoped_key(key(KeyCode::Enter)) {
            ScopedDashboardAction::Row {
                repository,
                action: DashboardAction::OpenPullRequest(url),
            } => {
                assert_eq!(repository.owner, "alpha");
                assert_eq!(repository.name, "api");
                assert!(url.ends_with("/pull/42"));
            }
            other => panic!("expected repository-scoped Open, got {other:?}"),
        }
    }
}

#[test]
fn global_rows_with_same_names_keep_distinct_paths_and_protect_each_mother() {
    use wisetree::tui::screens::dashboard::ScopedDashboardAction;
    let mut screen = global_screen(vec![
        global_row(row("/tmp/alpha/api", "main", true), "alpha", "api", true),
        global_row(row("/tmp/beta/web", "main", true), "beta", "web", true),
        global_row(row("/tmp/alpha/bug", "bug", false), "alpha", "api", true),
        global_row(row("/tmp/beta/bug", "bug", false), "beta", "web", true),
    ])
    .with_filter(wisetree::config::schema::DashboardFilter { groups: vec![] });
    for (owner, name) in [("alpha", "api"), ("beta", "web")] {
        match screen.handle_scoped_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)) {
            ScopedDashboardAction::Row {
                repository,
                action: DashboardAction::MotherWorktreeProtected,
            } => {
                assert_eq!(
                    (repository.owner.as_str(), repository.name.as_str()),
                    (owner, name)
                );
            }
            other => panic!("expected protected mother, got {other:?}"),
        }
        screen.handle_scoped_key(key(KeyCode::Down));
    }
    for owner in ["alpha", "beta"] {
        match screen.handle_scoped_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)) {
            ScopedDashboardAction::Row {
                repository,
                action: DashboardAction::JumpToDelete(path),
            } => {
                assert_eq!(repository.owner, owner);
                assert_eq!(path, format!("/tmp/{owner}/bug"));
                assert!(std::path::Path::new(&path).is_absolute());
            }
            other => panic!("expected scoped delete, got {other:?}"),
        }
        screen.handle_scoped_key(key(KeyCode::Down));
    }
}

#[test]
fn global_local_row_without_pr_keeps_status_metadata_and_general_commands() {
    let mut screen = global_screen(vec![global_row(
        row("/tmp/alpha/bug", "bug", false),
        "alpha",
        "api",
        true,
    )]);
    let rendered = dump_lines(240, 24, |f| screen.render(f, f.area()));
    let data = rendered
        .lines()
        .find(|line| line.contains("alpha") && line.contains("api"))
        .unwrap();
    for text in ["bug", "Dirty", "+1", "deadbee"] {
        assert!(data.contains(text), "{data}");
    }
    screen.handle_scoped_key(key(KeyCode::Enter));
    for label in [
        "Open with Command",
        "Copy path to clipboard",
        "Delete worktree",
    ] {
        assert!(screen.general_command_labels().contains(&label.to_string()));
    }
}

#[test]
fn global_bulk_delete_targets_exclude_remote_rows_and_all_mothers() {
    let mut local = global_row(
        row_with_pr_state("/tmp/alpha/bug", "bug", true, PrState::Open),
        "alpha",
        "api",
        true,
    );
    local.worktree.is_main = false;
    let remote = global_row(
        row_with_pr_state("", "remote", true, PrState::Open),
        "beta",
        "web",
        false,
    );
    let mother = global_row(
        row_with_pr_state("/tmp/alpha/api", "main", true, PrState::Open),
        "alpha",
        "api",
        true,
    );
    let mut screen = global_screen(vec![mother, local, remote]);
    screen.handle_key(key(KeyCode::Tab));
    // Merged → Closed → Drafted → Opened.
    for _ in 0..3 {
        screen.handle_key(key(KeyCode::Right));
    }
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        DashboardAction::BulkDelete(BulkDeleteStatus::Opened, vec!["/tmp/alpha/bug".into()])
    );
}

#[test]
fn global_bulk_delete_targets_respect_the_search_filter() {
    let rows = ["first", "second"]
        .into_iter()
        .map(|name| {
            global_row(
                row_with_pr_state(
                    &format!("/tmp/{name}/feature"),
                    "feature",
                    true,
                    PrState::Open,
                ),
                "owner",
                name,
                true,
            )
        })
        .collect();
    let mut screen = global_screen(rows);
    for c in "second".chars() {
        screen.handle_key(key(KeyCode::Char(c)));
    }
    screen.handle_key(key(KeyCode::Tab));
    for _ in 0..3 {
        screen.handle_key(key(KeyCode::Right));
    }
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        DashboardAction::BulkDelete(BulkDeleteStatus::Opened, vec!["/tmp/second/feature".into()])
    );
}

#[test]
fn global_empty_state_names_selected_accounts_and_counts_no_local_worktrees() {
    let mut screen = global_screen(vec![]);
    let rendered = dump(120, 24, |f| screen.render(f, f.area()));
    assert!(rendered.contains(wisetree::messages::GLOBAL_DASHBOARD_EMPTY));
    assert!(rendered.contains("0 worktrees, 0 dirty, 0 PRs open"));
}

#[test]
fn remote_open_mouse_click_preserves_repository_context() {
    use ratatui::layout::Position;
    use wisetree::tui::screens::dashboard::ScopedDashboardAction;
    let mut screen = global_screen(vec![global_row(
        row_with_pr("", "bug", true),
        "alpha",
        "api",
        false,
    )]);
    screen.handle_scoped_key(key(KeyCode::Enter));
    let backend = TestBackend::new(120, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| screen.render(f, f.area())).unwrap();
    let buffer = terminal.backend().buffer();
    let open = (0..buffer.area.height)
        .find_map(|y| {
            (0..buffer.area.width.saturating_sub(4)).find_map(|x| {
                let text = (x..x + 4)
                    .map(|column| buffer[(column, y)].symbol())
                    .collect::<String>();
                (text == "Open").then_some(Position::new(x, y))
            })
        })
        .expect("Open button rendered");
    match screen.handle_scoped_mouse_click(open) {
        ScopedDashboardAction::Row {
            repository,
            action: DashboardAction::OpenPullRequest(_),
        } => {
            assert_eq!(repository.owner, "alpha");
            assert_eq!(repository.name, "api");
        }
        other => panic!("expected scoped Open action, got {other:?}"),
    }
}

#[test]
fn global_checkout_dependent_pr_action_keeps_its_repository() {
    use wisetree::tui::screens::dashboard::ScopedDashboardAction;
    let mut screen = global_screen(vec![global_row(
        row_with_pr("/tmp/beta/bug", "bug", true),
        "beta",
        "web",
        true,
    )]);
    screen.handle_scoped_key(key(KeyCode::Enter));
    screen.handle_scoped_key(key(KeyCode::Tab));
    match screen.handle_scoped_key(key(KeyCode::Char('m'))) {
        ScopedDashboardAction::Row {
            repository,
            action: DashboardAction::MergePullRequest(request),
        } => {
            assert_eq!(repository.owner, "beta");
            assert_eq!(repository.name, "web");
            assert_eq!(request.worktree_path, "/tmp/beta/bug");
            assert_eq!(request.number, 42);
        }
        other => panic!("expected scoped Merge action, got {other:?}"),
    }
}

fn delete_shortcuts() -> [KeyEvent; 3] {
    [
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
    ]
}

fn shortcut_screen(global: bool, rows: Vec<DashboardRow>) -> DashboardScreen {
    if global {
        global_screen(
            rows.into_iter()
                .map(|row| global_row(row, "alpha", "api", true))
                .collect(),
        )
        .with_filter(wisetree::config::schema::DashboardFilter { groups: vec![] })
    } else {
        let mut screen = ready_screen(true);
        screen.set_rows(rows);
        screen
    }
}

fn type_query(screen: &mut DashboardScreen, query: &str) {
    for c in query.chars() {
        screen.handle_key(key(KeyCode::Char(c)));
    }
}

fn rendered_query(screen: &mut DashboardScreen) -> String {
    dump_lines(160, 30, |f| screen.render(f, f.area()))
        .lines()
        .find(|line| line.starts_with("Search:"))
        .unwrap()
        .to_string()
}

#[test]
fn delete_shortcuts_target_visible_row_and_preserve_query_in_both_scopes() {
    use wisetree::tui::screens::dashboard::ScopedDashboardAction;
    for global in [false, true] {
        for shortcut in delete_shortcuts() {
            for query in ["", "feat"] {
                let mut screen = shortcut_screen(
                    global,
                    vec![
                        row("/tmp/repo", "main", true),
                        row("/tmp/repo-bug", "bug", true),
                        row("/tmp/repo-feat", "feat", true),
                    ],
                );
                type_query(&mut screen, query);
                if query.is_empty() {
                    screen.handle_key(key(KeyCode::Down));
                    screen.handle_key(key(KeyCode::Down));
                }
                let before = rendered_query(&mut screen);
                let expected = DashboardAction::JumpToDelete("/tmp/repo-feat".into());
                if global {
                    match screen.handle_scoped_key(shortcut) {
                        ScopedDashboardAction::Row { repository, action } => {
                            assert_eq!(repository.owner, "alpha");
                            assert_eq!(action, expected);
                        }
                        other => panic!("expected scoped delete, got {other:?}"),
                    }
                } else {
                    assert_eq!(screen.handle_key(shortcut), expected);
                }
                assert_eq!(rendered_query(&mut screen), before);
            }
        }
    }
}

#[test]
fn delete_shortcuts_protect_mothers_and_reject_remote_rows_with_any_query() {
    use wisetree::tui::screens::dashboard::ScopedDashboardAction;
    for shortcut in delete_shortcuts() {
        for query in ["", "main"] {
            for global in [false, true] {
                let mut screen = shortcut_screen(global, vec![row("/tmp/repo", "main", true)]);
                type_query(&mut screen, query);
                let before = rendered_query(&mut screen);
                if global {
                    assert!(matches!(
                        screen.handle_scoped_key(shortcut),
                        ScopedDashboardAction::Row {
                            action: DashboardAction::MotherWorktreeProtected,
                            ..
                        }
                    ));
                } else {
                    assert_eq!(
                        screen.handle_key(shortcut),
                        DashboardAction::MotherWorktreeProtected
                    );
                }
                assert_eq!(rendered_query(&mut screen), before);
            }
        }
        for query in ["", "bug"] {
            let mut screen = global_screen(vec![global_row(
                row_with_pr("", "bug", true),
                "alpha",
                "api",
                false,
            )]);
            type_query(&mut screen, query);
            let before = rendered_query(&mut screen);
            assert_eq!(
                screen.handle_scoped_key(shortcut),
                ScopedDashboardAction::NoLocalWorktree
            );
            assert_eq!(screen.handle_key(shortcut), DashboardAction::Continue);
            assert_eq!(rendered_query(&mut screen), before);
        }
    }
}

#[test]
fn plain_deletion_keys_only_edit_search_and_word_deletion_still_works() {
    use wisetree::tui::screens::dashboard::ScopedDashboardAction;
    for global in [false, true] {
        for code in [KeyCode::Backspace, KeyCode::Delete] {
            let mut screen = shortcut_screen(global, vec![row("/tmp/repo-feat", "feat", true)]);
            let empty = rendered_query(&mut screen);
            assert_eq!(
                screen.handle_scoped_key(key(code)),
                ScopedDashboardAction::Dashboard(DashboardAction::Continue)
            );
            assert_eq!(rendered_query(&mut screen), empty);
            type_query(&mut screen, "feat");
            if code == KeyCode::Delete {
                screen.handle_key(key(KeyCode::Left));
            }
            assert_eq!(
                screen.handle_scoped_key(key(code)),
                ScopedDashboardAction::Dashboard(DashboardAction::Continue)
            );
            assert!(rendered_query(&mut screen).contains("Search: fea"));
        }
        for shortcut in [
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT),
        ] {
            let mut screen = shortcut_screen(global, vec![]);
            type_query(&mut screen, "feat bug");
            assert_eq!(screen.handle_key(shortcut), DashboardAction::Continue);
            // The Global Dashboard can render its Filter button beside the query.
            let rendered = rendered_query(&mut screen);
            assert_eq!(rendered.split("  ").next().unwrap().trim_end(), "Search: feat");
        }
    }
}

#[test]
fn delete_shortcuts_do_nothing_without_table_focus_or_visible_rows() {
    use wisetree::tui::screens::dashboard::ScopedDashboardAction;
    for global in [false, true] {
        for shortcut in delete_shortcuts() {
            for query in ["", "feat"] {
                for focus in [KeyCode::Tab, KeyCode::Enter] {
                    let mut screen =
                        shortcut_screen(global, vec![row("/tmp/repo-feat", "feat", true)]);
                    type_query(&mut screen, query);
                    screen.handle_key(key(focus));
                    assert_eq!(
                        screen.handle_scoped_key(shortcut),
                        ScopedDashboardAction::Dashboard(DashboardAction::Continue)
                    );
                }
            }
            let mut screen = shortcut_screen(global, vec![row("/tmp/repo-feat", "feat", true)]);
            type_query(&mut screen, "no-match");
            assert_eq!(
                screen.handle_scoped_key(shortcut),
                ScopedDashboardAction::Dashboard(DashboardAction::Continue)
            );
        }
    }
}

fn profile_filter(owner: &str) -> wisetree::config::schema::DashboardFilter {
    use wisetree::config::schema::{
        DashboardFilter, DashboardFilterField, DashboardFilterGroup, DashboardFilterOperator,
        DashboardFilterRule,
    };
    DashboardFilter {
        groups: vec![DashboardFilterGroup {
            rules: vec![DashboardFilterRule {
                field: DashboardFilterField::Profile,
                operator: DashboardFilterOperator::IsAnyOf,
                values: vec![owner.into()],
            }],
        }],
    }
}

#[test]
fn global_filter_button_stays_at_right_and_hover_clears_on_leave() {
    let mut screen = global_screen(vec![
        global_row(row("/tmp/alpha/api", "main", true), "alpha", "api", true),
        global_row(row("/tmp/alpha/bug", "bug", false), "alpha", "api", true),
    ]);
    for width in [160, 50, 20] {
        let rendered = dump_lines(width, 30, |f| screen.render(f, f.area()));
        assert!(rendered.lines().next().unwrap().ends_with("[ Filter (1) ]"));
        let button = screen.filter_button_rect().unwrap();
        assert_eq!(button.right(), width);
        assert_eq!(button.y, 0);
    }
    let plain = dump(160, 30, |f| screen.render(f, f.area()));
    assert!(plain.contains("1 of 2 worktrees, 1 dirty, 0 PRs open"));
    assert!(!plain.contains("Status  is none of"));
    let button = screen.filter_button_rect().unwrap();
    screen.handle_mouse_move(Position::new(button.x, button.y));
    let hovered = dump(160, 30, |f| screen.render(f, f.area()));
    assert!(hovered.contains("Status  is none of  Mother"));
    assert!(hovered.contains("Showing 1 of 2 worktrees · 1 hidden"));
    screen.handle_mouse_move(Position::new(0, 2));
    let left = dump(160, 30, |f| screen.render(f, f.area()));
    assert!(!left.contains("Status  is none of"));
    assert!(!left.contains("Showing 1 of 2 worktrees · 1 hidden"));
}

#[test]
fn global_default_filter_hides_mothers_but_repository_scope_does_not_filter() {
    let rows = vec![
        global_row(row("/tmp/alpha/api", "main", true), "alpha", "api", true),
        global_row(row("/tmp/alpha/bug", "bug", false), "alpha", "api", true),
    ];
    let mut screen = global_screen(rows.clone());
    assert_eq!(
        screen.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)),
        DashboardAction::JumpToDelete("/tmp/alpha/bug".into())
    );
    screen.set_rows(vec![rows[0].clone()]);
    let hidden = dump(160, 30, |f| screen.render(f, f.area()));
    assert!(hidden.contains(wisetree::messages::GLOBAL_DASHBOARD_FILTER_HIDDEN));
    assert!(hidden.contains("0 of 1 worktrees"));
    assert!(!hidden.contains(wisetree::messages::GLOBAL_DASHBOARD_EMPTY));

    let mut repository = DashboardScreen::new(
        true,
        true,
        true,
        vec!["branch".into(), "status".into()],
        vec![],
        true,
    )
    .with_filter(profile_filter("nobody"));
    repository.set_rows(rows);
    let rendered = dump(160, 30, |f| repository.render(f, f.area()));
    assert!(rendered.contains("Mother"));
    assert!(!rendered.contains("Filter ("));
    assert!(repository.filter_button_rect().is_none());
}

#[test]
fn global_filter_and_search_limit_batch_actions_selection_and_row_clicks() {
    let rows: Vec<_> = [("alpha", "bug"), ("beta", "bug"), ("alpha", "other")]
        .into_iter()
        .map(|(owner, branch)| {
            let row = row_with_pr_state(
                &format!("/tmp/{owner}/{branch}"),
                branch,
                true,
                PrState::Merged,
            );
            global_row(row, owner, "api", true)
        })
        .collect();
    {
        let mut screen = global_screen(rows.clone()).with_filter(profile_filter("alpha"));
        for ch in "bug".chars() {
            screen.handle_key(key(KeyCode::Char(ch)));
        }
        let rendered = dump_lines(160, 32, |f| screen.render(f, f.area()));
        assert!(rendered.contains("1 of 3 worktrees"));
        assert!(!rendered.contains("beta"));
        screen.handle_key(key(KeyCode::Tab));
        match screen.handle_key(key(KeyCode::Enter)) {
            DashboardAction::BulkDelete(_, paths) => assert_eq!(paths, vec!["/tmp/alpha/bug"]),
            other => panic!("unexpected action: {other:?}"),
        }
    }
    let mut screen = global_screen(rows.clone()).with_filter(profile_filter("alpha"));
    screen.handle_key(key(KeyCode::Down));
    // A refresh hides the selected row and clamps to the remaining visible row.
    screen.set_rows(rows[..2].to_vec());
    assert_eq!(
        screen.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)),
        DashboardAction::JumpToDelete("/tmp/alpha/bug".into())
    );
    let rendered = dump_lines(160, 32, |f| screen.render(f, f.area()));
    let y = rendered
        .lines()
        .position(|line| line.contains("➤") && line.contains("bug"))
        .unwrap() as u16;
    screen.handle_mouse_click(Position::new(2, y));
    assert!(dump(160, 32, |f| screen.render(f, f.area())).contains("bug"));
    // Navigate from the clicked row's action menu.
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        DashboardAction::NavigateTo("/tmp/alpha/bug".into())
    );
}

#[test]
fn global_tooltip_groups_truncation_inactive_loading_and_modal_suppression() {
    use wisetree::config::schema::{
        DashboardFilter, DashboardFilterField as F, DashboardFilterOperator as O,
        DashboardFilterRule,
    };
    let mut filter = profile_filter("alpha");
    filter.groups[0].rules.push(DashboardFilterRule {
        field: F::Branch,
        operator: O::Contains,
        values: vec!["bug".into()],
    });
    let mut other = profile_filter("beta").groups.remove(0);
    other.rules[0].values = (0..30)
        .map(|n| format!("very-long-profile-value-{n}"))
        .collect();
    filter.groups.push(other);
    assert!(filter
        .tooltip_lines(26)
        .iter()
        .any(|line| line == "+30 more"));
    let mut screen = global_screen(vec![global_row(
        row("/tmp/alpha/bug", "bug", false),
        "alpha",
        "api",
        true,
    )])
    .with_filter(filter);
    dump(120, 30, |f| screen.render(f, f.area()));
    let button = screen.filter_button_rect().unwrap();
    screen.handle_mouse_move(Position::new(button.x, button.y));
    let hovered = dump(120, 30, |f| screen.render(f, f.area()));
    for text in ["AND", "── OR ──", "+29 more", "Filter (3)"] {
        assert!(hovered.contains(text), "{text}: {hovered}");
    }
    screen.handle_key(key(KeyCode::Enter));
    assert!(!dump(120, 30, |f| screen.render(f, f.area())).contains("── OR ──"));

    let mut screen = global_screen(vec![]).with_filter(DashboardFilter { groups: vec![] });
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| screen.render(f, f.area())).unwrap();
    let button = screen.filter_button_rect().unwrap();
    assert_eq!(
        terminal.backend().buffer()[(button.x, button.y)].fg,
        colors::MUTED
    );
    screen.handle_mouse_move(Position::new(button.x, button.y));
    assert!(dump(120, 30, |f| screen.render(f, f.area()))
        .contains(wisetree::messages::GLOBAL_FILTER_NONE));

    let mut screen = DashboardScreen::new(true, true, true, vec![], vec![], true)
        .with_scope(wisetree::tui::screens::dashboard::DashboardScope::Global);
    let loading = dump_lines(120, 30, |f| screen.render(f, f.area()));
    assert!(loading.contains("0 of 0 worktrees"));
    assert!(loading.lines().next().unwrap().ends_with("[ Filter (1) ]"));
    let button = screen.filter_button_rect().unwrap();
    screen.handle_mouse_move(Position::new(button.x, button.y));
    for (width, height) in [(120, 30), (30, 5), (8, 3), (1, 1)] {
        dump(width, height, |f| screen.render(f, f.area()));
    }
}

#[test]
fn global_filter_tooltip_is_suppressed_by_close_pr_modal() {
    let mut screen = global_screen(vec![global_row(
        row_with_pr("/tmp/alpha/bug", "bug", true),
        "alpha",
        "api",
        true,
    )]);
    screen.handle_key(key(KeyCode::Enter));
    let menu = dump_lines(180, 35, |f| screen.render(f, f.area()));
    let (x, y) = menu
        .lines()
        .enumerate()
        .find_map(|(y, line)| {
            line.find("Close")
                .map(|byte| (line[..byte].chars().count() as u16, y as u16))
        })
        .expect("Close PR command");
    screen.handle_mouse_click(Position::new(x, y));
    dump(180, 35, |f| screen.render(f, f.area()));
    let button = screen.filter_button_rect().unwrap();
    screen.handle_mouse_move(Position::new(button.x, button.y));
    let modal = dump(180, 35, |f| screen.render(f, f.area()));
    assert!(modal.contains("Close Pull Request"));
    assert!(!modal.contains("Status  is none of"));
    assert!(!modal.contains("Showing 1 of 1"));
}

fn filter_ctrl_f(screen: &mut DashboardScreen) {
    screen.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
}

fn filter_type(screen: &mut DashboardScreen, value: &str) {
    for c in value.chars() {
        assert_eq!(
            screen.handle_key(key(KeyCode::Char(c))),
            DashboardAction::Continue
        );
    }
}

fn filter_choose_field(screen: &mut DashboardScreen, name: &str) {
    filter_type(screen, name);
    screen.handle_key(key(KeyCode::Enter)); // field
    screen.handle_key(key(KeyCode::Enter)); // first valid operator
}

fn filter_button(screen: &mut DashboardScreen, label: &str) -> DashboardAction {
    // Exercise the same mouse hit regions as the application, including after resize.
    let rendered = dump_lines(120, 35, |f| screen.render(f, f.area()));
    let (y, line) = rendered
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(label))
        .unwrap();
    let x = line.find(label).unwrap();
    screen.handle_mouse_click(Position::new(x as u16, y as u16))
}

#[test]
fn global_filter_editor_builds_and_or_rules_applies_rows_and_persists_json() {
    use wisetree::config::{
        schema::{
            DashboardFilter, DashboardFilterField as F, DashboardFilterGroup,
            DashboardFilterOperator as O, DashboardFilterRule,
        },
        ConfigService,
    };
    use wisetree::tui::screens::dashboard::ScopedDashboardAction;
    let rows = vec![
        global_row(
            row_with_pr_state("/tmp/merged-release", "release/one", true, PrState::Merged),
            "a",
            "r",
            true,
        ),
        global_row(
            row_with_pr_state("/tmp/closed-release", "release/two", true, PrState::Closed),
            "a",
            "r",
            true,
        ),
        global_row(
            row_with_pr_state("/tmp/merged-other", "other", true, PrState::Merged),
            "a",
            "r",
            true,
        ),
        global_row(
            row_with_check("/tmp/failed", "fix", true, CheckStatus::Failed),
            "a",
            "r",
            true,
        ),
        global_row(row("/tmp/clean", "clean", true), "a", "r", true),
    ];
    let mut screen = global_screen(rows.clone());
    filter_ctrl_f(&mut screen);
    filter_button(&mut screen, "Clear all");
    screen.handle_key(key(KeyCode::Char('a')));
    filter_choose_field(&mut screen, "Status");
    screen.handle_key(key(KeyCode::Down)); // Merged
    screen.handle_key(key(KeyCode::Char(' ')));
    for _ in 0..3 {
        screen.handle_key(key(KeyCode::Down));
    } // Closed
    screen.handle_key(key(KeyCode::Char(' ')));
    let rendered = dump(120, 35, |f| screen.render(f, f.area()));
    assert!(rendered.contains("☒ Merged (2)"));
    assert!(rendered.contains("☒ Closed (1)"));
    screen.handle_key(key(KeyCode::Enter));
    screen.handle_key(key(KeyCode::Char('a')));
    filter_choose_field(&mut screen, "Branch");
    filter_type(&mut screen, "release");
    screen.handle_key(key(KeyCode::Enter));
    screen.handle_key(key(KeyCode::Char('o')));
    filter_choose_field(&mut screen, "Checks");
    screen.handle_key(key(KeyCode::Down)); // Failed
    screen.handle_key(key(KeyCode::Char(' ')));
    screen.handle_key(key(KeyCode::Enter));
    let rendered = dump_lines(120, 35, |f| screen.render(f, f.area()));
    assert!(rendered.contains("Showing 3 of 5 worktrees"));
    assert_eq!(rendered.matches("Match ALL of").count(), 2);
    assert!(rendered.contains("OR"));
    // Latest polling data changes counts without losing the draft.
    screen.set_rows(rows[..4].to_vec());
    assert!(dump(120, 35, |f| screen.render(f, f.area())).contains("Showing 3 of 4 worktrees"));
    // Tab from the focused rule reaches + Rule, then Apply after four more Tabs.
    for _ in 0..5 {
        screen.handle_key(key(KeyCode::Tab));
    }
    let action = screen.handle_scoped_key(key(KeyCode::Enter));
    let rule = |field, operator, values: &[&str]| DashboardFilterRule {
        field,
        operator,
        values: values.iter().map(|v| v.to_string()).collect(),
    };
    let expected = DashboardFilter {
        groups: vec![
            DashboardFilterGroup {
                rules: vec![
                    rule(F::Status, O::IsAnyOf, &["Merged", "Closed"]),
                    rule(F::Branch, O::Contains, &["release"]),
                ],
            },
            DashboardFilterGroup {
                rules: vec![rule(F::Checks, O::IsAnyOf, &["Failed"])],
            },
        ],
    };
    assert_eq!(
        action,
        ScopedDashboardAction::Dashboard(DashboardAction::PersistFilter(expected.clone()))
    );
    assert_eq!(screen.filter(), &expected);
    let rendered = dump(160, 35, |f| screen.render(f, f.area()));
    for name in ["merged-release", "closed-release", "failed"] {
        assert!(rendered.contains(name));
    }
    assert!(!rendered.contains("merged-other"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".wisetree/config.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"{"extra":true,"globalDashboard":{"selectedAccounts":["a"],"future":42}}"#,
    )
    .unwrap();
    let ScopedDashboardAction::Dashboard(DashboardAction::PersistFilter(filter)) = action else {
        panic!("missing persistence action");
    };
    ConfigService::update_global_dashboard_filter_at(&path, filter).unwrap();
    let document: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        document["globalDashboard"]["filter"],
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(
        document["globalDashboard"]["selectedAccounts"],
        serde_json::json!(["a"])
    );
    assert_eq!(document["globalDashboard"]["future"], 42);
    assert_eq!(document["extra"], true);
}

#[test]
fn global_filter_editor_cancel_unchanged_apply_clear_and_reset() {
    use wisetree::config::{schema::DashboardFilter, ConfigService};
    let mut screen = global_screen(vec![global_row(
        row("/tmp/mother", "main", true),
        "a",
        "r",
        true,
    )]);
    // Mouse opens the editor.
    dump(120, 35, |f| screen.render(f, f.area()));
    let button = screen.filter_button_rect().unwrap();
    screen.handle_mouse_click(Position::new(button.x, button.y));
    assert_eq!(
        filter_button(&mut screen, "Apply"),
        DashboardAction::Continue
    );
    filter_ctrl_f(&mut screen);
    filter_button(&mut screen, "Clear all");
    assert_eq!(
        filter_button(&mut screen, "Cancel"),
        DashboardAction::Continue
    );
    assert_eq!(screen.filter(), &DashboardFilter::default());
    filter_ctrl_f(&mut screen);
    filter_button(&mut screen, "Clear all");
    assert_eq!(
        filter_button(&mut screen, "Apply"),
        DashboardAction::PersistFilter(DashboardFilter { groups: vec![] })
    );
    assert!(dump(120, 35, |f| screen.render(f, f.area())).contains("mother"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    ConfigService::update_global_dashboard_filter_at(&path, screen.filter().clone()).unwrap();
    assert!(ConfigService::load_global_dashboard_preferences_at(&path)
        .unwrap()
        .filter
        .groups
        .is_empty());
    filter_ctrl_f(&mut screen);
    filter_button(&mut screen, "Reset to default");
    assert_eq!(
        filter_button(&mut screen, "Apply"),
        DashboardAction::PersistFilter(DashboardFilter::default())
    );
    // Removing the last rule removes its group and produces the explicit empty filter.
    filter_ctrl_f(&mut screen);
    screen.handle_key(key(KeyCode::Backspace));
    assert_eq!(
        filter_button(&mut screen, "Apply"),
        DashboardAction::PersistFilter(DashboardFilter { groups: vec![] })
    );
}

#[test]
fn global_filter_editor_validates_inputs_supports_empty_rows_and_preserves_search_cursor() {
    let mut screen = global_screen(vec![]);
    filter_type(&mut screen, "ab");
    screen.handle_key(key(KeyCode::Left));
    filter_ctrl_f(&mut screen);
    screen.handle_key(key(KeyCode::Char('a')));
    filter_choose_field(&mut screen, "Status");
    let rendered = dump(120, 35, |f| screen.render(f, f.area()));
    assert!(rendered.contains("☐ Merged (0)"));
    screen.handle_key(key(KeyCode::Enter));
    assert!(dump(120, 35, |f| screen.render(f, f.area()))
        .contains(wisetree::messages::GLOBAL_FILTER_VALUE_ERROR));
    // Esc goes back through value → operator → field → overview → dashboard.
    for _ in 0..4 {
        screen.handle_key(key(KeyCode::Esc));
    }
    screen.handle_key(key(KeyCode::Right));
    filter_type(&mut screen, "c");
    assert!(dump(120, 35, |f| screen.render(f, f.area())).contains("Search: abc"));
    filter_ctrl_f(&mut screen);
    screen.handle_key(key(KeyCode::Char('a')));
    filter_choose_field(&mut screen, "Ahead");
    filter_type(&mut screen, "abc");
    assert!(dump(120, 35, |f| screen.render(f, f.area()))
        .contains(wisetree::messages::GLOBAL_FILTER_INTEGER_ERROR));
    screen.handle_key(key(KeyCode::Enter));
    assert!(dump(120, 35, |f| screen.render(f, f.area())).contains("3/3"));
    for _ in 0..3 {
        screen.handle_key(key(KeyCode::Backspace));
    }
    filter_type(&mut screen, "12");
    screen.handle_key(key(KeyCode::Enter));
    filter_button(&mut screen, "Apply");
    assert_eq!(screen.filter().groups[0].rules[1].values, vec!["12"]);
    filter_ctrl_f(&mut screen);
    screen.handle_key(key(KeyCode::Char('o')));
    filter_choose_field(&mut screen, "Profile");
    filter_type(&mut screen, "custom-owner");
    screen.handle_key(key(KeyCode::Enter));
    filter_button(&mut screen, "Apply");
    assert_eq!(
        screen.filter().groups[1].rules[0].values,
        vec!["custom-owner"]
    );
    // Loading does not prevent opening the editor; new rows update its counts.
    let mut loading = DashboardScreen::new(false, false, false, vec![], vec![], true)
        .with_scope(wisetree::tui::screens::dashboard::DashboardScope::Global);
    filter_ctrl_f(&mut loading);
    assert!(dump(120, 35, |f| loading.render(f, f.area())).contains("Showing 0 of 0"));
    loading.set_rows(vec![row("/tmp/fresh", "feature", true)]);
    assert!(dump(120, 35, |f| loading.render(f, f.area())).contains("Showing 1 of 1"));
}

#[test]
fn global_filter_editor_mouse_rule_edit_and_text_suggestions() {
    let mut screen = global_screen(vec![global_row(
        row("/tmp/release", "release/next", true),
        "a",
        "r",
        true,
    )]);
    filter_ctrl_f(&mut screen);
    // Clicking a rule begins the same guided editing flow as Enter.
    let rendered = dump_lines(120, 35, |f| screen.render(f, f.area()));
    let y = rendered
        .lines()
        .position(|line| line.contains("Status  is none of  Mother"))
        .unwrap();
    screen.handle_mouse_click(Position::new(3, y as u16));
    assert!(dump(120, 35, |f| screen.render(f, f.area())).contains("1/3"));
    // Switch the existing rule to Branch contains, then complete a suggestion.
    filter_choose_field(&mut screen, "Branch");
    screen.handle_key(key(KeyCode::Enter)); // empty text cannot save
    assert!(dump(120, 35, |f| screen.render(f, f.area()))
        .contains(wisetree::messages::GLOBAL_FILTER_VALUE_ERROR));
    filter_type(&mut screen, "release");
    assert!(dump(120, 35, |f| screen.render(f, f.area())).contains("release/next"));
    screen.handle_key(key(KeyCode::Tab));
    screen.handle_key(key(KeyCode::Enter));
    filter_button(&mut screen, "Apply");
    assert_eq!(
        screen.filter().groups[0].rules[0].values,
        vec!["release/next"]
    );
    filter_ctrl_f(&mut screen);
    screen.handle_key(key(KeyCode::Char('a')));
    filter_choose_field(&mut screen, "Checks");
    screen.handle_key(key(KeyCode::Down));
    screen.handle_key(key(KeyCode::Char(' '))); // Failed
    screen.set_rows(vec![]);
    assert!(dump(120, 35, |f| screen.render(f, f.area())).contains("☒ Failed (0)"));
    // Resizing preserves selection and never invalidates rule/editor state.
    for (width, height) in [(20, 10), (60, 20), (120, 35)] {
        dump(width, height, |f| screen.render(f, f.area()));
    }
    screen.handle_key(key(KeyCode::Enter));
    filter_button(&mut screen, "Apply");
    assert_eq!(screen.filter().groups[0].rules[1].values, vec!["Failed"]);
}

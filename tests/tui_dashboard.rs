use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
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
    assert!(rendered.contains("0 worktrees, 0 dirty, 1 PR open"));
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
fn remote_backspace_reports_no_local_worktree_but_still_edits_search() {
    use wisetree::tui::screens::dashboard::ScopedDashboardAction;
    let mut screen = global_screen(vec![global_row(
        row_with_pr("", "bug", true),
        "alpha",
        "api",
        false,
    )]);
    for code in [KeyCode::Backspace, KeyCode::Delete] {
        assert_eq!(
            screen.handle_scoped_key(key(code)),
            ScopedDashboardAction::NoLocalWorktree
        );
        // Even the legacy entry point cannot issue a delete for a remote row.
        assert_eq!(screen.handle_key(key(code)), DashboardAction::Continue);
    }
    screen.handle_scoped_key(key(KeyCode::Char('b')));
    assert_eq!(
        screen.handle_scoped_key(key(KeyCode::Backspace)),
        ScopedDashboardAction::Dashboard(DashboardAction::Continue)
    );
}

#[test]
fn global_rows_with_same_names_keep_distinct_paths_and_protect_each_mother() {
    use wisetree::tui::screens::dashboard::ScopedDashboardAction;
    let mut screen = global_screen(vec![
        global_row(row("/tmp/alpha/api", "main", true), "alpha", "api", true),
        global_row(row("/tmp/beta/web", "main", true), "beta", "web", true),
        global_row(row("/tmp/alpha/bug", "bug", false), "alpha", "api", true),
        global_row(row("/tmp/beta/bug", "bug", false), "beta", "web", true),
    ]);
    for (owner, name) in [("alpha", "api"), ("beta", "web")] {
        match screen.handle_scoped_key(key(KeyCode::Backspace)) {
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
        match screen.handle_scoped_key(key(KeyCode::Backspace)) {
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

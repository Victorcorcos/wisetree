use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

use wisetree::config::schema::{AiHarness, AiModelConfig, AiSplitConfig};
use wisetree::messages::colors;
use wisetree::services::{
    parse_split_plan, ChangeUnit, ChangeUnitKind, SplitDraft, SplitDraftJobStatus,
    SplitDraftProgress, SplitDraftRecord, SplitIdentity, SplitPlanResult, SplitPreflight,
    SplitPublication, SplitPublishedPullRequest, SplitRepositorySnapshot,
};
use wisetree::tui::screens::dashboard::SplitRequest;
use wisetree::tui::screens::split_pr::{SplitAction, SplitPullRequestScreen, SplitStep};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn ai(model: &str, thinking: &str) -> AiModelConfig {
    AiModelConfig {
        model: model.into(),
        thinking: thinking.into(),
        harness: AiHarness::OpenCode,
    }
}

fn config() -> AiSplitConfig {
    AiSplitConfig {
        plan: ai("openai/strong-planner", "high"),
        open: ai("openai/fast-writer", "low"),
    }
}

fn request(number: Option<u64>) -> SplitRequest {
    SplitRequest {
        branch: "feature/large-change".into(),
        worktree_path: "/tmp/repo-large-change".into(),
        base_ref: Some("upstream/main".into()),
        pr_base_ref: number.map(|_| "main".into()),
        number,
        title: number.map(|_| "Large change".into()),
        url: number.map(|number| format!("https://github.com/acme/repo/pull/{number}")),
    }
}

fn render(screen: &mut SplitPullRequestScreen, width: u16, height: u16) -> (String, bool) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| screen.render(frame, frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    let mut split_colored = false;
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            text.push_str(cell.symbol());
            split_colored |= cell.symbol() == "S" && cell.fg == colors::SPLIT;
        }
        text.push('\n');
    }
    (text, split_colored)
}

fn render_buffer(screen: &mut SplitPullRequestScreen, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| screen.render(frame, frame.area()))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn review_screen() -> SplitPullRequestScreen {
    let unit = |id: &str, path: &str, additions: u64, deletions: u64| ChangeUnit {
        id: id.into(),
        path: path.into(),
        old_path: None,
        kind: ChangeUnitKind::TextHunk,
        old_start: Some(1),
        old_lines: Some(deletions),
        new_start: Some(1),
        new_lines: Some(additions),
        additions,
        deletions,
        line_counts_available: true,
    };
    let preflight = SplitPreflight {
        worktree_path: "/tmp/repo-large-change".into(),
        identity: SplitIdentity {
            repository: "acme/repo".into(),
            remote: "upstream".into(),
            base_ref: "upstream/main".into(),
            base_sha: "base-sha".into(),
            source_branch: "feature/large-change".into(),
            source_head: "source-sha".into(),
            max: 10,
            additions: 8,
            deletions: 2,
        },
        units: vec![
            unit("CU0001", "src/a.rs", 3, 1),
            unit("CU0002", "tests/a_test.rs", 1, 0),
            unit("CU0003", "src/b.rs", 2, 1),
            unit("CU0004", "tests/b_test.rs", 2, 0),
        ],
    };
    let response = r#"{"responsibilities":[{"order":1,"name":"Add the shared foundation","branch_slug":"shared-foundation","rationale":"Applies directly to the resolved base and has no stack dependency.","units":["CU0001","CU0002"],"test_units":["CU0002"],"paths":["src/a.rs","tests/a_test.rs"]},{"order":2,"name":"Add the foundation consumer","branch_slug":"foundation-consumer","rationale":"Depends on the shared foundation behavior from PR 1.","units":["CU0003","CU0004"],"test_units":["CU0004"],"paths":["src/b.rs","tests/b_test.rs"]}]}"#;
    let plan = parse_split_plan(response, &preflight).unwrap();
    let snapshot = SplitRepositorySnapshot {
        status: String::new(),
        head: "source-sha".into(),
        refs: "refs/heads/feature source-sha".into(),
        files: Vec::new(),
    };
    let mut screen = SplitPullRequestScreen::new(request(None), config());
    screen.set_preflight(preflight);
    screen.show_plan(SplitPlanResult { plan, snapshot });
    screen
}

fn publication() -> SplitPublication {
    SplitPublication {
        repository: "acme/repo".into(),
        trunk: "main".into(),
        source_branch: "feature/large-change".into(),
        stack_link_completed: true,
        status: "published, verified, and provisionally titled".into(),
        diagnostics: None,
        pull_requests: vec![
            SplitPublishedPullRequest {
                order: 1,
                branch: "feature/large-change.1_foundation".into(),
                expected_base: "main".into(),
                number: 91,
                url: "https://github.com/acme/repo/pull/91".into(),
                provisional_title: "Feature Large Change (1/2)".into(),
                provisional_title_applied: true,
            },
            SplitPublishedPullRequest {
                order: 2,
                branch: "feature/large-change".into(),
                expected_base: "feature/large-change.1_foundation".into(),
                number: 92,
                url: "https://github.com/acme/repo/pull/92".into(),
                provisional_title: "Feature Large Change (2/2)".into(),
                provisional_title_applied: true,
            },
        ],
    }
}

fn focus_confirm(screen: &mut SplitPullRequestScreen) {
    screen.handle_key(key(KeyCode::Tab));
    screen.handle_key(key(KeyCode::Left));
}

#[test]
fn overview_names_the_full_sequence_roles_max_and_new_top_pr() {
    let mut screen = SplitPullRequestScreen::new(request(None), config());
    let (text, split_colored) = render(&mut screen, 120, 52);
    assert!(
        split_colored,
        "Split heading/modal should use its own color"
    );
    for expected in [
        "Will run:",
        "Revalidate the worktree",
        "SRP stack",
        "Approve or reject the plan",
        "local branches and worktrees",
        "flag layers over",
        "gh stack link",
        "concurrently",
        "Apply the final metadata",
        // Centered AI roles table, one row per configured role.
        "Role",
        "Thinking",
        "Harness",
        "Permission",
        "openai/strong-planner",
        "openai/fast-writer",
        // MAX lives in the bordered `options` group as an editable field.
        "options",
        "MAX",
        "1000",
        "a guideline",
        "create a new top pull request",
    ] {
        assert!(text.contains(expected), "missing {expected:?}:\n{text}");
    }
}

#[test]
fn active_source_pr_is_shown_as_the_reused_top_pr() {
    let mut screen = SplitPullRequestScreen::new(request(Some(91)), config());
    let (text, _) = render(&mut screen, 120, 52);
    assert!(text.contains("Top PR"), "{text}");
    assert!(text.contains("reuse #91 — Large change"), "{text}");
    assert!(
        text.contains("https://github.com/acme/repo/pull/91"),
        "{text}"
    );
}

#[test]
fn max_accepts_only_positive_u64_values() {
    for invalid in ["", "   ", "0", "-1", "ten", "18446744073709551616"] {
        let mut screen = SplitPullRequestScreen::new(request(None), config());
        screen.set_max_input(invalid);
        focus_confirm(&mut screen);
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            SplitAction::Continue,
            "{invalid:?} advanced"
        );
        assert_eq!(screen.step(), SplitStep::Confirm);
    }

    let mut screen = SplitPullRequestScreen::new(request(None), config());
    screen.set_max_input("250");
    focus_confirm(&mut screen);
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SplitAction::Confirmed(250)
    );
}

#[test]
fn missing_model_is_readable_and_confirmation_fails_safely() {
    let mut cfg = config();
    cfg.plan.model = "  ".into();
    let mut screen = SplitPullRequestScreen::new(request(None), cfg);
    let (text, _) = render(&mut screen, 100, 48);
    assert!(text.contains("(not configured)"), "{text}");

    focus_confirm(&mut screen);
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SplitAction::Continue
    );
    let (text, _) = render(&mut screen, 100, 48);
    assert!(text.contains("dashboard.ai.split.plan"), "{text}");
    assert!(text.contains(".wisetree.json"), "{text}");
}

#[test]
fn confirm_hands_validated_max_to_preflight_and_cancel_stays_mutation_free() {
    let mut confirmed = SplitPullRequestScreen::new(request(None), config());
    focus_confirm(&mut confirmed);
    assert_eq!(
        confirmed.handle_key(key(KeyCode::Enter)),
        SplitAction::Confirmed(1000)
    );
    confirmed.start_preflight(1000);
    assert_eq!(confirmed.step(), SplitStep::Preflight);
    assert_eq!(confirmed.confirmed_max(), Some(1000));

    let mut cancelled = SplitPullRequestScreen::new(request(None), config());
    assert_eq!(
        cancelled.handle_key(key(KeyCode::Esc)),
        SplitAction::Cancelled
    );
    assert_eq!(cancelled.step(), SplitStep::Confirm);
    assert_eq!(cancelled.confirmed_max(), None);
}

#[test]
fn confirm_page_fits_details_roles_max_and_buttons_on_a_small_terminal() {
    let mut screen = SplitPullRequestScreen::new(request(None), config());
    let (text, _) = render(&mut screen, 80, 40);
    for expected in [
        "Split this branch",
        "Branch",
        "Will run:",
        "openai/fast-writer",
        "options",
        "MAX",
        "1000",
        "Confirm",
        "Cancel",
    ] {
        assert!(text.contains(expected), "missing {expected:?}:\n{text}");
    }
}

#[test]
fn max_field_is_typed_in_place_and_only_takes_digits() {
    let mut screen = SplitPullRequestScreen::new(request(None), config());
    for _ in 0..4 {
        screen.handle_key(key(KeyCode::Backspace));
    }
    assert_eq!(screen.max_input(), "");
    for code in [
        KeyCode::Char('2'),
        KeyCode::Char('x'),
        KeyCode::Char('5'),
        KeyCode::Char('0'),
    ] {
        screen.handle_key(key(code));
    }
    assert_eq!(screen.max_input(), "250");

    let (text, _) = render(&mut screen, 100, 40);
    assert!(text.contains("250"), "{text}");

    focus_confirm(&mut screen);
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SplitAction::Confirmed(250)
    );
}

#[test]
fn an_empty_max_reports_the_problem_inside_the_options_group() {
    let mut screen = SplitPullRequestScreen::new(request(None), config());
    screen.set_max_input("");
    focus_confirm(&mut screen);
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SplitAction::Continue
    );
    let (text, _) = render(&mut screen, 100, 40);
    assert!(text.contains("MAX must be a positive integer"), "{text}");
}

#[test]
fn review_renders_complete_bottom_to_top_integrity_and_keyboard_actions() {
    let mut screen = review_screen();
    let (text, split_colored) = render(&mut screen, 110, 36);
    assert!(split_colored);
    for expected in [
        "bottom to top",
        "Source branch",
        "feature/large-change · commit source-sha",
        "Stack base",
        "upstream/main · commit base-sha",
        "Size guideline",
        "up to 10 changed lines per PR",
        "soft limit; responsibility boundaries take",
        "priority",
        "╭─ PR 1 · Add the shared foundation",
        "Field",
        "Details",
        "Responsibility",
        "Add the shared foundation",
        "Dependency",
        "Applies directly to the resolved base and has no stack dependency.",
        "src/a.rs, tests/a_test.rs",
        "Changes",
        "src/a.rs:1-3 (+3 -1)",
        "tests/a_test.rs:1 (+1 -0)",
        "Related tests",
        "tests/a_test.rs:1 (+1 -0)",
        "Integrity",
        "+4 -1 = 5 · within MAX 10 ✓",
        "Aggregate integrity",
        "all 4 changed sections accounted for",
        "+8 -2 = 10 source lines",
        "Approve",
        "Reject",
    ] {
        assert!(text.contains(expected), "missing {expected:?}:\n{text}");
    }
    assert!(text.contains("│ Field"), "{text}");
    assert!(text.contains("╰────────────────"), "{text}");
    assert!(!text.contains("CU0001"), "{text}");
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SplitAction::Approved
    );
}

#[test]
fn reject_feedback_ignores_empty_supports_multiline_and_escape_preserves_plan() {
    let mut screen = review_screen();
    screen.handle_key(key(KeyCode::Right));
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SplitAction::Continue
    );
    assert_eq!(screen.step(), SplitStep::Feedback);
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SplitAction::Continue
    );
    screen.handle_paste("Move the API unit down\nand keep its test with it");
    match screen.handle_key(key(KeyCode::Enter)) {
        SplitAction::Rejected(feedback) => {
            assert!(feedback.contains("Move the API unit down\n"));
        }
        other => panic!("expected rejection, got {other:?}"),
    }

    let revised = screen
        .approval_payload()
        .map(|(_, plan, snapshot)| SplitPlanResult {
            plan: plan.clone(),
            snapshot: snapshot.clone(),
        })
        .unwrap();
    screen.show_plan(revised);
    screen.handle_key(key(KeyCode::Right));
    screen.handle_key(key(KeyCode::Enter));
    screen.handle_key(key(KeyCode::Char('x')));
    assert_eq!(screen.handle_key(key(KeyCode::Esc)), SplitAction::Continue);
    assert_eq!(screen.step(), SplitStep::Review);
    assert!(screen.plan_json().is_some());
}

#[test]
fn review_buttons_are_clickable_and_second_contract_failure_requires_explicit_retry() {
    let mut screen = review_screen();
    let buffer = render_buffer(&mut screen, 100, 30);
    let approve = (buffer.area.height.saturating_sub(3)..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
        .find(|&(x, y)| buffer[(x, y)].symbol() == "A")
        .expect("Approve button");
    assert_eq!(
        screen.handle_mouse_click(ratatui::layout::Position::new(approve.0, approve.1)),
        SplitAction::Approved
    );

    screen.set_planning_error("malformed JSON".into(), true);
    assert_eq!(screen.step(), SplitStep::Error);
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SplitAction::RetryPlanning
    );
}

#[test]
fn publication_progress_and_verified_stack_are_visible() {
    let mut screen = review_screen();
    screen.start_approving();
    assert_eq!(screen.handle_key(key(KeyCode::Esc)), SplitAction::Continue);
    let (progress, _) = render(&mut screen, 100, 20);
    assert!(progress.contains("materializing, publishing, and verifying"));

    screen.mark_approved(publication());
    screen.update_draft_progress(SplitDraftProgress {
        operation_id: 7,
        generation: 4,
        layer: 1,
        pr_number: 91,
        status: SplitDraftJobStatus::Drafted,
        activity: Some("draft one complete".into()),
        error: None,
    });
    let (drafting, _) = render(&mut screen, 100, 20);
    assert!(drafting.contains("1/2 completed"), "{drafting}");
    // Each row says, in plain language, whether that PR's title and
    // description are already live on GitHub.
    assert!(
        drafting.contains("PR #91 · drafted, not yet on GitHub"),
        "{drafting}"
    );
    assert!(drafting.contains("PR #92 · queued"), "{drafting}");
    assert!(drafting.contains("AI Activity"), "{drafting}");
    screen.handle_key(key(KeyCode::Down));
    let (selected, _) = render(&mut screen, 100, 20);
    assert!(selected.contains("Launching the selected drafting AI"));

    screen.finish_drafting(vec![SplitDraftRecord {
        job_id: "source-layer-1-pr-91".into(),
        source_head: "source-sha".into(),
        order: 1,
        pr_number: 91,
        pr_url: "https://github.com/acme/repo/pull/91".into(),
        correction_attempted: false,
        draft: Some(SplitDraft {
            title_summary: "Foundation".into(),
            body_content: "# Description ✍️\n\nDetails".into(),
        }),
        final_title: Some("Foundation (1/2)".into()),
        final_body: Some("body".into()),
        applied: true,
        error: None,
    }]);
    assert_eq!(screen.step(), SplitStep::Complete);
    let (done, _) = render(&mut screen, 100, 20);
    assert!(done.contains("Published and verified 2 stacked pull requests"));
    assert!(
        done.contains("https://github.com/acme/repo/pull/91"),
        "{done}"
    );
    assert!(done.contains("feature/large-change.1_foundation"), "{done}");
    assert!(
        done.contains("MAX 10 per layer: every layer within it"),
        "{done}"
    );
    // The done page names the title that is now live on GitHub, per PR, so
    // "which PRs got their metadata rewritten" is answerable at a glance.
    assert!(done.contains("title: Foundation (1/2)"), "{done}");
    assert!(done.contains("AI title + description applied"), "{done}");
    assert!(
        done.contains("not drafted — still the provisional title on GitHub"),
        "{done}"
    );
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SplitAction::Finished
    );

    screen.set_publication_error("push rejected exactly".into());
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SplitAction::RetryPublication
    );
}

/// The planning stage is the one the developer watches and steers, so it
/// renders the same embedded-terminal panel the other AI-assisted commands
/// use rather than a passive log.
#[test]
fn planning_shows_the_embedded_terminal_panel_and_its_focus_controls() {
    let mut screen = SplitPullRequestScreen::new(request(None), config());
    screen.start_planning(false);
    assert_eq!(screen.step(), SplitStep::Planning);
    assert!(!screen.has_pty());

    let (text, _) = render(&mut screen, 100, 20);
    let buffer = render_buffer(&mut screen, 100, 20);
    let accented = (0..buffer.area.height)
        .any(|y| (0..buffer.area.width).any(|x| buffer[(x, y)].fg == colors::SPLIT));
    assert!(text.contains("Planning the semantic stack"), "{text}");
    assert!(text.contains("AI Activity"), "{text}");
    assert!(
        text.contains("Launching the selected planning AI"),
        "{text}"
    );
    assert!(text.contains("Focus:"), "{text}");
    assert!(text.contains("Outer (wisetree)"), "{text}");
    assert!(text.contains("Tab"), "{text}");
    assert!(text.contains("Cancel planning"), "{text}");
    assert!(accented, "the planning page keeps the Split accent");

    let (corrective, _) = {
        screen.start_planning(true);
        render(&mut screen, 100, 20)
    };
    assert!(
        corrective.contains("Correcting the planning response contract"),
        "{corrective}"
    );
}

/// Without a live child there is nothing to focus, so Tab is inert while the
/// outer keys keep scrolling the planner and cancelling the run.
#[test]
fn planning_keys_scroll_the_planner_and_cancel_without_touching_the_plan() {
    let mut screen = SplitPullRequestScreen::new(request(None), config());
    screen.start_planning(false);

    assert_eq!(screen.handle_key(key(KeyCode::Tab)), SplitAction::Continue);
    assert_eq!(
        screen.handle_key(key(KeyCode::PageUp)),
        SplitAction::WritePty(b"\x1b[5~".to_vec())
    );
    assert_eq!(
        screen.handle_key(key(KeyCode::PageDown)),
        SplitAction::WritePty(b"\x1b[6~".to_vec())
    );
    assert_eq!(screen.handle_key(key(KeyCode::Esc)), SplitAction::Cancelled);

    // A planning failure tears the terminal down so no orphan child survives.
    screen.start_planning(false);
    screen.set_planning_error("planner exited".into(), true);
    assert!(!screen.has_pty());
    assert_eq!(screen.step(), SplitStep::Error);
}

/// An oversized layer must be impossible to miss on the review page: the
/// banner counts them, each one states its overflow, and the reason it was
/// kept whole stays next to it so Approve/Reject is an informed choice.
#[test]
fn review_flags_every_pull_request_that_runs_past_max_with_its_reason() {
    let mut screen = review_screen();
    let mut preflight = screen.preflight().expect("preflight").clone();
    preflight.identity.max = 3;
    screen.set_preflight(preflight);

    let buffer = render_buffer(&mut screen, 110, 40);
    let mut text = String::new();
    let mut addition_is_green = false;
    let mut deletion_is_red = false;
    let mut total_is_yellow = false;
    let mut srp_explanation_is_teal = false;
    for y in 0..buffer.area.height {
        let row = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>();
        addition_is_green |= row.contains("+4")
            && (0..buffer.area.width)
                .any(|x| buffer[(x, y)].symbol() == "+" && buffer[(x, y)].fg == colors::SUCCESS);
        deletion_is_red |= row.contains("-1")
            && (0..buffer.area.width)
                .any(|x| buffer[(x, y)].symbol() == "-" && buffer[(x, y)].fg == colors::ERROR);
        total_is_yellow |= row.contains("= 5 · 2 over MAX 3.")
            && (0..buffer.area.width)
                .any(|x| buffer[(x, y)].symbol() == "=" && buffer[(x, y)].fg == colors::WARNING);
        srp_explanation_is_teal |= row.contains("Single Responsibility")
            && (0..buffer.area.width)
                .any(|x| buffer[(x, y)].symbol() == "S" && buffer[(x, y)].fg == colors::INFO);
        text.push_str(&row);
        text.push('\n');
    }
    assert!(text.contains("up to 3 changed lines per PR"), "{text}");
    assert!(text.contains("2 of 2 pull requests exceed MAX 3"), "{text}");
    assert!(
        text.contains("Responsibility boundaries win over size"),
        "{text}"
    );
    assert!(text.contains("+4 -1 = 5 · 2 over MAX 3."), "{text}");
    assert!(
        text.contains("Kept whole to preserve Single Responsibility"),
        "{text}"
    );
    assert!(text.contains("Principle across Pull Requests."), "{text}");
    assert!(addition_is_green, "oversized additions should be green");
    assert!(deletion_is_red, "oversized deletions should be red");
    assert!(total_is_yellow, "oversized total should be yellow");
    assert!(
        srp_explanation_is_teal,
        "oversized SRP explanation should be teal"
    );
    assert!(text.contains("Responsibility"), "{text}");
    assert!(text.contains("Dependency"), "{text}");
    // Approving an oversized plan is still allowed — it is the point.
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SplitAction::Approved
    );
}

#[test]
fn review_reports_layers_within_max_as_within_the_guideline() {
    let mut screen = review_screen();
    let buffer = render_buffer(&mut screen, 110, 40);
    let mut text = String::new();
    let mut addition_is_green = false;
    let mut deletion_is_red = false;
    let mut within_max_is_blue = false;
    for y in 0..buffer.area.height {
        let row = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>();
        addition_is_green |= row.contains("+4")
            && (0..buffer.area.width)
                .any(|x| buffer[(x, y)].symbol() == "+" && buffer[(x, y)].fg == colors::SUCCESS);
        deletion_is_red |= row.contains("-1")
            && (0..buffer.area.width)
                .any(|x| buffer[(x, y)].symbol() == "-" && buffer[(x, y)].fg == colors::ERROR);
        within_max_is_blue |= row.contains("within MAX 10 ✓")
            && (0..buffer.area.width)
                .any(|x| buffer[(x, y)].symbol() == "w" && buffer[(x, y)].fg == colors::INFO);
        text.push_str(&row);
        text.push('\n');
    }
    assert!(text.contains("within MAX 10 ✓"), "{text}");
    assert!(!text.contains("exceed MAX"), "{text}");
    assert!(!text.contains("Over MAX"), "{text}");
    assert!(addition_is_green, "additions should be green");
    assert!(deletion_is_red, "deletions should be red");
    assert!(within_max_is_blue, "within-MAX status should be blue");
}

/// The decision pair uses the same palette as every other pull-request
/// command: green to approve, red to reject — never the command accent, which
/// would make the safe and the destructive choice look identical.
#[test]
fn approve_is_green_and_reject_is_red_in_both_border_and_label() {
    let mut screen = review_screen();
    let buffer = render_buffer(&mut screen, 100, 30);
    let color_of = |needle: char| {
        (buffer.area.height.saturating_sub(3)..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .find(|&(x, y)| buffer[(x, y)].symbol() == needle.to_string())
            .map(|(x, y)| buffer[(x, y)].fg)
            .expect("button label")
    };
    // Approve is focused by default, so its border carries the accent too.
    assert_eq!(color_of('A'), colors::SUCCESS);
    assert_eq!(color_of('R'), colors::ERROR);
    assert_ne!(colors::SUCCESS, colors::SPLIT);
    assert_ne!(colors::ERROR, colors::SPLIT);

    let border_colors: Vec<_> = (buffer.area.height.saturating_sub(3)..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
        .filter(|&(x, y)| buffer[(x, y)].symbol() == "─")
        .map(|(x, y)| buffer[(x, y)].fg)
        .collect();
    assert!(
        border_colors.contains(&colors::SUCCESS),
        "focused Approve border is green"
    );
    assert!(
        border_colors.contains(&colors::MUTED),
        "unfocused Reject border is muted"
    );
}

/// The drafting stage fans out one AI call per pull request, so the only way
/// to know which PRs already carry their new metadata is the per-PR row. Every
/// state must read as plain language and carry a distinguishing color.
#[test]
fn drafting_rows_say_which_pull_requests_are_already_updated_on_github() {
    let mut screen = review_screen();
    screen.mark_approved(publication());
    let progress = |layer: usize, pr_number: u64, status: SplitDraftJobStatus| SplitDraftProgress {
        operation_id: 1,
        generation: 1,
        layer,
        pr_number,
        status,
        activity: None,
        error: None,
    };
    screen.update_draft_progress(progress(1, 91, SplitDraftJobStatus::Applied));
    screen.update_draft_progress(progress(2, 92, SplitDraftJobStatus::Failed));

    let buffer = render_buffer(&mut screen, 110, 20);
    let mut text = String::new();
    let mut applied_is_green = false;
    let mut failed_is_red = false;
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            text.push_str(cell.symbol());
            applied_is_green |= cell.symbol() == "✓" && cell.fg == colors::SUCCESS;
            failed_is_red |= cell.symbol() == "f" && cell.fg == colors::ERROR;
        }
        text.push('\n');
    }
    assert!(
        text.contains("PR #91 · title + description updated ✓"),
        "{text}"
    );
    assert!(
        text.contains("PR #92 · failed — provisional title kept"),
        "{text}"
    );
    assert!(text.contains("1/2 completed"), "{text}");
    assert!(applied_is_green, "an updated PR reads as success");
    assert!(failed_is_red, "a failed PR reads as an error");
}

/// One failed drafting job must not hide the five that worked: the board stays
/// up with the failed PR selected, its error readable, and a retry that only
/// re-runs the incomplete work.
#[test]
fn a_failed_drafting_job_stays_on_the_board_and_is_retryable_in_place() {
    let mut screen = review_screen();
    screen.mark_approved(publication());
    let progress = |layer: usize, pr_number: u64, status, error: Option<&str>| SplitDraftProgress {
        operation_id: 1,
        generation: 1,
        layer,
        pr_number,
        status,
        activity: None,
        error: error.map(str::to_string),
    };
    screen.update_draft_progress(progress(1, 91, SplitDraftJobStatus::Applied, None));
    screen.update_draft_progress(progress(
        2,
        92,
        SplitDraftJobStatus::Failed,
        Some("gh pr edit: server error"),
    ));
    screen.set_drafting_error(
        "Split metadata update for PR #92 failed; successful updates were retained.".into(),
    );

    // Still the board, not a bare error page.
    assert_eq!(screen.step(), SplitStep::Drafting);
    let (text, _) = render(&mut screen, 110, 24);
    assert!(text.contains("PR #92 failed"), "{text}");
    assert!(
        text.contains("1/2 pull requests already carry their AI metadata"),
        "{text}"
    );
    assert!(
        text.contains("re-runs only what is still incomplete"),
        "{text}"
    );
    // The successful PR keeps its result visible next to the failure.
    assert!(
        text.contains("PR #91 · title + description updated ✓"),
        "{text}"
    );
    assert!(
        text.contains("PR #92 · failed — provisional title kept"),
        "{text}"
    );
    // The failed job is auto-selected, so its error is the visible stream.
    assert!(text.contains("gh pr edit: server error"), "{text}");

    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SplitAction::RetryDrafting
    );
    assert_eq!(
        screen.handle_key(key(KeyCode::Char('r'))),
        SplitAction::RetryDrafting
    );
    screen.resume_drafting();
    assert!(screen.drafting_error().is_none());
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SplitAction::Continue,
        "a running board must not retry on Enter"
    );
}

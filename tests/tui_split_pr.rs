use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use wisetree::config::schema::{AiHarness, AiModelConfig, AiSplitConfig};
use wisetree::messages::colors;
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
        "Deterministically revalidate",
        "SRP plan",
        "Approve/Reject loop",
        "local branches and worktrees",
        "parent-to-child diff against MAX",
        "gh stack link",
        "drafting AI concurrently",
        "Compose titles",
        "Apply the final PR metadata",
        "once per proposal",
        "once per resulting PR (concurrently)",
        "MAX is additions plus deletions, including tests",
        "MAX: 1000",
        "create a new top pull request",
    ] {
        assert!(text.contains(expected), "missing {expected:?}:\n{text}");
    }
}

#[test]
fn active_source_pr_is_shown_as_the_reused_top_pr() {
    let mut screen = SplitPullRequestScreen::new(request(Some(91)), config());
    let (text, _) = render(&mut screen, 120, 52);
    assert!(text.contains("Top PR: reuse #91 — Large change"), "{text}");
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
    assert!(text.contains("plan: (not configured)"), "{text}");

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
fn narrow_terminal_can_scroll_from_overview_to_roles_and_max() {
    let mut screen = SplitPullRequestScreen::new(request(None), config());
    let (top, _) = render(&mut screen, 44, 24);
    assert!(top.contains("Split this branch"), "{top}");
    screen.handle_key(key(KeyCode::End));
    let (bottom, _) = render(&mut screen, 44, 24);
    assert!(bottom.contains("openai/fast-writer"), "{bottom}");
    assert!(bottom.contains("MAX: 1000"), "{bottom}");
    assert!(bottom.contains("Confirm"), "{bottom}");
    assert!(bottom.contains("Cancel"), "{bottom}");
}

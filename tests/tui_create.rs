//! State-machine + render tests for the Create Worktree screen.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use wisetree::git::types::GitBranch;
use wisetree::tui::screens::create::{CreateAction, CreateScreen, CreateStep};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn type_str(screen: &mut CreateScreen, s: &str) {
    for c in s.chars() {
        screen.handle_key(key(KeyCode::Char(c)));
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

fn branches() -> Vec<GitBranch> {
    vec![
        GitBranch {
            name: "main".into(),
            commit: "deadbeef".into(),
            last_used: None,
            is_current: true,
            is_default: true,
            is_remote: false,
        },
        GitBranch {
            name: "develop".into(),
            commit: "cafef00d".into(),
            last_used: None,
            is_current: false,
            is_default: false,
            is_remote: false,
        },
        GitBranch {
            name: "origin/feat/login".into(),
            commit: "1234abcd".into(),
            last_used: None,
            is_current: false,
            is_default: false,
            is_remote: true,
        },
    ]
}

fn ready_screen() -> CreateScreen {
    let mut s = CreateScreen::new();
    s.set_branches(branches());
    s
}

#[test]
fn create_initial_step_is_directory_and_loading_until_branches_arrive() {
    let s = CreateScreen::new();
    assert_eq!(s.step(), CreateStep::Directory);
    assert!(s.loading());
}

#[test]
fn loading_render_shows_loading_branches_message() {
    let s = CreateScreen::new();
    let dumped = dump(60, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("Loading branches"));
}

#[test]
fn typing_directory_then_enter_advances_to_source_branch() {
    let mut s = ready_screen();
    type_str(&mut s, "feat-login");
    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, CreateAction::Continue);
    assert_eq!(s.step(), CreateStep::SourceBranch);
    assert_eq!(s.directory_name, "feat-login");
}

#[test]
fn invalid_directory_name_pins_error_and_stays() {
    let mut s = ready_screen();
    type_str(&mut s, "../escape");
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), CreateStep::Directory);
}

#[test]
fn esc_on_directory_step_cancels() {
    let mut s = ready_screen();
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, CreateAction::Cancelled);
}

#[test]
fn select_source_branch_advances_to_new_branch_with_directory_default() {
    let mut s = ready_screen();
    type_str(&mut s, "feat-login");
    s.handle_key(key(KeyCode::Enter)); // → source-branch
                                       // Branches sort alphabetically: develop, main, origin/feat/login. The first row
                                       // is selected by default, so Enter picks "develop".
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), CreateStep::NewBranch);
    assert_eq!(s.source_branch, "develop");
}

#[test]
fn custom_ref_path_takes_user_to_input() {
    let mut s = ready_screen();
    type_str(&mut s, "feat-login");
    s.handle_key(key(KeyCode::Enter));
    // Branches sort alphabetically: develop (default-selected), main, origin/feat/login,
    // then the custom-ref entry. Three Downs from `develop` reach the custom-ref row.
    for _ in 0..3 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), CreateStep::CustomRef);
    type_str(&mut s, "v1.2.3");
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), CreateStep::NewBranch);
    assert_eq!(s.source_branch, "v1.2.3");
}

#[test]
fn empty_new_branch_falls_back_to_source_for_local() {
    let mut s = ready_screen();
    type_str(&mut s, "myfeat");
    s.handle_key(key(KeyCode::Enter));
    // First option is "develop" (alphabetical) — picked by default.
    s.handle_key(key(KeyCode::Enter));
    // new-branch step has default="myfeat"; clear it then submit blank
    for _ in 0..6 {
        s.handle_key(key(KeyCode::Backspace));
    }
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), CreateStep::Confirm);
    assert_eq!(s.new_branch, "develop");
}

#[test]
fn empty_new_branch_strips_remote_prefix() {
    let mut s = ready_screen();
    type_str(&mut s, "loginfeat");
    s.handle_key(key(KeyCode::Enter));
    // Branches sort alphabetically: develop (default-selected), main, origin/feat/login.
    // Two Downs from `develop` land on origin/feat/login.
    s.handle_key(key(KeyCode::Down));
    s.handle_key(key(KeyCode::Down));
    s.handle_key(key(KeyCode::Enter));
    // clear "loginfeat" default
    for _ in 0..9 {
        s.handle_key(key(KeyCode::Backspace));
    }
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), CreateStep::Confirm);
    assert_eq!(s.source_branch, "origin/feat/login");
    assert_eq!(s.new_branch, "feat/login");
}

#[test]
fn new_branch_existing_local_blocks_with_validation() {
    let mut s = ready_screen();
    type_str(&mut s, "feat-x");
    s.handle_key(key(KeyCode::Enter));
    // First option is "develop" — skip past it to "main" so we can try to reuse the
    // "develop" name on the new-branch step without auto-matching the source.
    s.handle_key(key(KeyCode::Down));
    s.handle_key(key(KeyCode::Enter));
    for _ in 0..6 {
        s.handle_key(key(KeyCode::Backspace));
    }
    type_str(&mut s, "develop");
    s.handle_key(key(KeyCode::Enter));
    // Should still be on new-branch with error pinned.
    assert_eq!(s.step(), CreateStep::NewBranch);
}

#[test]
fn confirm_with_y_then_enter_yields_confirmed_action() {
    let mut s = ready_screen();
    type_str(&mut s, "feat-x");
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Enter)); // first option, alphabetical: "develop"
    s.handle_key(key(KeyCode::Enter)); // accept default branch name
    s.handle_key(key(KeyCode::Char('y')));
    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, CreateAction::Continue);
    assert_eq!(s.step(), CreateStep::NavigateConfirm);
    // Default on the navigate page is Yes — Enter accepts and emits Confirmed.
    let action = s.handle_key(key(KeyCode::Enter));
    match action {
        CreateAction::Confirmed {
            directory_name,
            source_branch,
            new_branch,
        } => {
            assert_eq!(directory_name, "feat-x");
            assert_eq!(source_branch, "develop");
            assert_eq!(new_branch, "feat-x");
        }
        other => panic!("expected Confirmed, got {other:?}"),
    }
    assert!(s.navigate_after_create);
}

#[test]
fn navigate_confirm_no_choice_still_proceeds_with_create() {
    let mut s = ready_screen();
    type_str(&mut s, "feat-x");
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('y')));
    s.handle_key(key(KeyCode::Enter)); // → NavigateConfirm
    assert_eq!(s.step(), CreateStep::NavigateConfirm);
    s.handle_key(key(KeyCode::Char('n')));
    let action = s.handle_key(key(KeyCode::Enter));
    assert!(matches!(action, CreateAction::Confirmed { .. }));
    assert!(!s.navigate_after_create);
}

#[test]
fn navigate_confirm_esc_cancels_entire_flow() {
    let mut s = ready_screen();
    type_str(&mut s, "feat-x");
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('y')));
    s.handle_key(key(KeyCode::Enter)); // → NavigateConfirm
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, CreateAction::Cancelled);
}

#[test]
fn start_creating_then_running_then_complete_renders_each_step() {
    let mut s = ready_screen();
    s.start_creating();
    let dumped = dump(60, 6, |f| s.render(f, f.area()));
    assert!(dumped.contains("Creating worktree"));

    s.start_running_commands(vec!["bun install".into(), "bun build".into()]);
    s.post_create_progress("bun install", 0);
    let dumped = dump(60, 8, |f| s.render(f, f.area()));
    assert!(dumped.contains("Running post-create commands (0/2)"));
    assert!(dumped.contains("bun install"));

    s.post_create_progress("bun build", 1);
    s.mark_complete();
    let dumped = dump(60, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("Worktree created successfully"));
}

#[test]
fn success_step_done_action_on_enter() {
    let mut s = ready_screen();
    s.mark_complete();
    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, CreateAction::Done);
}

#[test]
fn source_branch_options_pin_priority_remotes_to_top() {
    let mut s = CreateScreen::new();
    s.set_branches(vec![
        GitBranch {
            name: "zeta".into(),
            commit: "1".into(),
            last_used: None,
            is_current: false,
            is_default: false,
            is_remote: false,
        },
        GitBranch {
            name: "origin/master".into(),
            commit: "2".into(),
            last_used: None,
            is_current: false,
            is_default: false,
            is_remote: true,
        },
        GitBranch {
            name: "alpha".into(),
            commit: "3".into(),
            last_used: None,
            is_current: false,
            is_default: false,
            is_remote: false,
        },
        GitBranch {
            name: "origin/main".into(),
            commit: "4".into(),
            last_used: None,
            is_current: false,
            is_default: false,
            is_remote: true,
        },
        GitBranch {
            name: "upstream/master".into(),
            commit: "5".into(),
            last_used: None,
            is_current: false,
            is_default: false,
            is_remote: true,
        },
        GitBranch {
            name: "upstream/main".into(),
            commit: "6".into(),
            last_used: None,
            is_current: false,
            is_default: false,
            is_remote: true,
        },
    ]);

    let opts = s.branch_options();
    let names: Vec<&str> = opts.iter().map(|o| o.value.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "upstream/main",
            "upstream/master",
            "origin/main",
            "origin/master",
            "alpha",
            "zeta",
            "__CUSTOM_REF__",
        ]
    );
}

#[test]
fn error_overlay_shows_message_and_clears_on_keypress() {
    let mut s = ready_screen();
    s.set_error("boom".into());
    let dumped = dump(60, 6, |f| s.render(f, f.area()));
    assert!(dumped.contains("boom"));
    assert!(dumped.contains("Press any key to try again"));
    s.handle_key(key(KeyCode::Char('x')));
    assert!(s.error().is_none());
}

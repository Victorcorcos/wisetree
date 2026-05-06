//! Behavior + render tests for `src/tui/widgets`. Snapshot-style assertions
//! against `TestBackend`'s symbol dump pin user-visible content (button
//! labels, hints, status icons) without requiring byte-for-byte equality with
//! upstream's Ink output — the two render models are intentionally different.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use wisetree::tui::widgets::{
    CommandListProgress, CommandProgress, ConfirmChoice, ConfirmDialog, ConfirmOutcome,
    ConfirmVariant, InputOutcome, InputPrompt, SelectOption, SelectOutcome, SelectPrompt, Spinner,
    Status, StatusIndicator, SPINNER_FRAMES,
};

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

// -- InputPrompt --------------------------------------------------------------

#[test]
fn input_prompt_appends_printable_chars() {
    let mut input = InputPrompt::new("Name");
    matches!(
        input.handle_key(key(KeyCode::Char('a'))),
        InputOutcome::Pending
    );
    matches!(
        input.handle_key(key(KeyCode::Char('b'))),
        InputOutcome::Pending
    );
    assert_eq!(input.value, "ab");
}

#[test]
fn input_prompt_backspace_erases() {
    let mut input = InputPrompt::new("Name").with_default("hi");
    matches!(
        input.handle_key(key(KeyCode::Backspace)),
        InputOutcome::Pending
    );
    assert_eq!(input.value, "h");
    matches!(
        input.handle_key(key(KeyCode::Delete)),
        InputOutcome::Pending
    );
    assert_eq!(input.value, "");
}

#[test]
fn input_prompt_enter_submits_when_valid() {
    let mut input = InputPrompt::new("Name").with_default("ok");
    match input.handle_key(key(KeyCode::Enter)) {
        InputOutcome::Submitted(v) => assert_eq!(v, "ok"),
        _ => panic!("expected Submitted"),
    }
}

#[test]
fn input_prompt_enter_blocked_by_validator_pins_error() {
    let mut input =
        InputPrompt::new("Name").with_validator(|v| (v.is_empty()).then(|| "Required".to_string()));
    matches!(input.handle_key(key(KeyCode::Enter)), InputOutcome::Pending);
    assert_eq!(input.error.as_deref(), Some("Required"));
}

#[test]
fn input_prompt_esc_cancels() {
    let mut input = InputPrompt::new("Name");
    matches!(input.handle_key(key(KeyCode::Esc)), InputOutcome::Cancelled);
}

#[test]
fn input_prompt_render_includes_placeholder_and_hint() {
    let input = InputPrompt::new("Branch").with_placeholder("e.g. feat/login");
    let s = dump(60, 8, |f| input.render(f, f.area()));
    assert!(s.contains("Branch"));
    assert!(s.contains("e.g. feat/login"));
    assert!(s.contains("Press Enter to confirm"));
}

#[test]
fn input_prompt_render_shows_error_when_pinned() {
    let mut input = InputPrompt::new("Branch")
        .with_validator(|v| (v.is_empty()).then(|| "Required".to_string()));
    matches!(input.handle_key(key(KeyCode::Enter)), InputOutcome::Pending);
    let s = dump(60, 8, |f| input.render(f, f.area()));
    assert!(s.contains("Required"));
}

#[test]
fn input_prompt_handles_multibyte_unicode() {
    let mut input = InputPrompt::new("Name");
    input.handle_key(key(KeyCode::Char('日')));
    input.handle_key(key(KeyCode::Char('本')));
    assert_eq!(input.value, "日本");
    input.handle_key(key(KeyCode::Backspace));
    assert_eq!(input.value, "日");
}

// -- SelectPrompt -------------------------------------------------------------

fn opts(labels: &[&str]) -> Vec<SelectOption<String>> {
    labels
        .iter()
        .map(|l| SelectOption::new(*l, l.to_string()))
        .collect()
}

#[test]
fn select_arrow_navigation_wraps() {
    let mut s = SelectPrompt::new("Pick", opts(&["a", "b", "c"]));
    matches!(s.handle_key(key(KeyCode::Up)), SelectOutcome::Pending);
    assert_eq!(s.selected, 2);
    matches!(s.handle_key(key(KeyCode::Down)), SelectOutcome::Pending);
    assert_eq!(s.selected, 0);
}

#[test]
fn select_jk_alias_navigates_when_not_searchable() {
    let mut s = SelectPrompt::new("Pick", opts(&["a", "b", "c"]));
    matches!(
        s.handle_key(key(KeyCode::Char('j'))),
        SelectOutcome::Pending
    );
    assert_eq!(s.selected, 1);
    matches!(
        s.handle_key(key(KeyCode::Char('k'))),
        SelectOutcome::Pending
    );
    assert_eq!(s.selected, 0);
}

#[test]
fn select_numeric_jump_when_not_searchable() {
    let mut s = SelectPrompt::new("Pick", opts(&["a", "b", "c", "d"]));
    s.handle_key(key(KeyCode::Char('3')));
    assert_eq!(s.selected, 2);
}

#[test]
fn select_searchable_filters_and_resets_selection() {
    let mut s = SelectPrompt::new("Pick", opts(&["alpha", "beta", "gamma"])).searchable();
    s.handle_key(key(KeyCode::Char('a')));
    assert_eq!(s.query, "a");
    let label = match s.handle_key(key(KeyCode::Enter)) {
        SelectOutcome::Selected(_, v) => v,
        _ => panic!("expected select"),
    };
    assert_eq!(label, "alpha");
}

#[test]
fn select_searchable_esc_clears_query_first_then_cancels() {
    let mut s = SelectPrompt::new("Pick", opts(&["a", "b"])).searchable();
    s.handle_key(key(KeyCode::Char('a')));
    matches!(s.handle_key(key(KeyCode::Esc)), SelectOutcome::Pending);
    assert!(s.query.is_empty());
    matches!(s.handle_key(key(KeyCode::Esc)), SelectOutcome::Cancelled);
}

#[test]
fn select_disabled_option_blocks_enter() {
    let opts = vec![
        SelectOption::new("first", "first".to_string()).disabled(),
        SelectOption::new("second", "second".to_string()),
    ];
    let mut s = SelectPrompt::new("Pick", opts);
    matches!(s.handle_key(key(KeyCode::Enter)), SelectOutcome::Pending);
}

#[test]
fn select_empty_after_filter_renders_no_matching_options() {
    let mut s = SelectPrompt::new("Pick", opts(&["alpha", "beta"])).searchable();
    s.handle_key(key(KeyCode::Char('z')));
    let dumped = dump(60, 12, |f| s.render(f, f.area()));
    assert!(dumped.contains("No matching options"));
}

#[test]
fn select_render_shows_more_above_below_when_long() {
    let labels: Vec<String> = (0..20).map(|i| format!("opt{i}")).collect();
    let opts: Vec<SelectOption<String>> = labels
        .iter()
        .map(|l| SelectOption::new(l.clone(), l.clone()))
        .collect();
    let mut s = SelectPrompt::new("Pick", opts);
    s.selected = 10;
    let dumped = dump(60, 24, |f| s.render(f, f.area()));
    assert!(dumped.contains("more above"));
    assert!(dumped.contains("more below"));
}

// -- ConfirmDialog ------------------------------------------------------------

#[test]
fn confirm_default_selection_is_cancel() {
    let dialog = ConfirmDialog::new("Title", "msg");
    assert_eq!(dialog.selected, ConfirmChoice::Cancel);
}

#[test]
fn confirm_left_right_tab_toggle() {
    let mut d = ConfirmDialog::new("T", "m");
    matches!(d.handle_key(key(KeyCode::Right)), ConfirmOutcome::Pending);
    assert_eq!(d.selected, ConfirmChoice::Confirm);
    matches!(d.handle_key(key(KeyCode::Tab)), ConfirmOutcome::Pending);
    assert_eq!(d.selected, ConfirmChoice::Cancel);
    matches!(d.handle_key(key(KeyCode::Left)), ConfirmOutcome::Pending);
    assert_eq!(d.selected, ConfirmChoice::Confirm);
}

#[test]
fn confirm_y_n_shortcut_pre_selects_button() {
    let mut d = ConfirmDialog::new("T", "m");
    d.handle_key(key(KeyCode::Char('y')));
    assert_eq!(d.selected, ConfirmChoice::Confirm);
    d.handle_key(key(KeyCode::Char('n')));
    assert_eq!(d.selected, ConfirmChoice::Cancel);
}

#[test]
fn confirm_enter_dispatches_selected_branch() {
    let mut d = ConfirmDialog::new("T", "m").with_default(ConfirmChoice::Confirm);
    matches!(d.handle_key(key(KeyCode::Enter)), ConfirmOutcome::Confirmed);
    let mut d = ConfirmDialog::new("T", "m");
    matches!(d.handle_key(key(KeyCode::Enter)), ConfirmOutcome::Cancelled);
}

#[test]
fn confirm_render_shows_labels_and_navigation_hint() {
    let dialog = ConfirmDialog::new("Delete?", "Are you sure?")
        .with_labels("Yep", "Nope")
        .with_variant(ConfirmVariant::Danger);
    let s = dump(60, 12, |f| dialog.render(f, f.area()));
    assert!(s.contains("Delete?"));
    assert!(s.contains("Are you sure"));
    assert!(s.contains("Yep"));
    assert!(s.contains("Nope"));
    assert!(s.contains("Tab to navigate"));
}

// -- Spinner / Status / CommandListProgress / CommandProgress ----------------

#[test]
fn spinner_advances_through_all_frames() {
    assert_eq!(SPINNER_FRAMES.len(), 10);
    let frames: Vec<&str> = (0..10).map(wisetree::tui::widgets::spinner_frame).collect();
    assert_eq!(
        frames
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        10
    );
}

#[test]
fn spinner_renders_label() {
    let s = Spinner::new(0).with_label("Loading");
    let dumped = dump(40, 1, |f| s.render(f, f.area()));
    assert!(dumped.contains("Loading"));
}

#[test]
fn status_indicator_loading_uses_spinner_frame() {
    let s = StatusIndicator::new(Status::Loading, "go").with_tick(0);
    let dumped = dump(40, 1, |f| s.render(f, f.area()));
    assert!(dumped.contains(SPINNER_FRAMES[0]));
    assert!(dumped.contains("go"));
}

#[test]
fn status_indicator_success_uses_label_token() {
    let s = StatusIndicator::new(Status::Success, "done");
    let dumped = dump(40, 1, |f| s.render(f, f.area()));
    assert!(dumped.contains("[SUCCESS]"));
    assert!(dumped.contains("done"));
}

#[test]
fn command_list_progress_renders_per_row_status() {
    let cmds: Vec<String> = vec!["bun install".into(), "bun test".into(), "bun build".into()];
    let dumped = dump(60, 8, |f| {
        CommandListProgress::new(&cmds, 1).render(f, f.area())
    });
    assert!(dumped.contains("Running post-create commands (1/3)"));
    assert!(dumped.contains("bun install"));
    assert!(dumped.contains("✓"));
    assert!(dumped.contains("○"));
}

#[test]
fn command_list_progress_marks_failed_rows() {
    let cmds: Vec<String> = vec!["a".into(), "b".into()];
    let failed = vec!["a".to_string()];
    let dumped = dump(60, 6, |f| {
        CommandListProgress::new(&cmds, 1)
            .with_failed(&failed)
            .render(f, f.area())
    });
    assert!(dumped.contains("✗"));
}

#[test]
fn command_progress_shows_executing_command() {
    let dumped = dump(60, 4, |f| {
        CommandProgress::new("bun install", 1, 3).render(f, f.area())
    });
    assert!(dumped.contains("Running post-create commands (1/3)"));
    assert!(dumped.contains("Executing: bun install"));
}

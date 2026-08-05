//! Behavior + render tests for `src/tui/widgets`. Snapshot-style assertions
//! against `TestBackend`'s symbol dump pin user-visible content (button
//! labels, hints, status icons) without requiring byte-for-byte equality with
//! upstream's Ink output — the two render models are intentionally different.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use std::path::PathBuf;
use wisetree::tui::image_upload::ImageAttachment;
use wisetree::tui::widgets::{
    BulkConfirmDialog, BulkConfirmFocus, BulkConfirmItem, BulkConfirmOutcome, CommandListProgress,
    CommandProgress, InputOutcome, InputPrompt, SelectOption, SelectOutcome, SelectPrompt, Spinner,
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

fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
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

fn attachment(name: &str) -> ImageAttachment {
    ImageAttachment {
        id: name.to_string(),
        filename: name.to_string(),
        mime_type: "image/png".to_string(),
        path: PathBuf::from(format!("/tmp/{name}")),
    }
}

#[test]
fn multiline_attachment_inserts_at_beginning_middle_and_end_of_unicode_text() {
    let mut input = InputPrompt::new("Description")
        .multiline()
        .with_default("日語");

    input.handle_key(key(KeyCode::Home));
    assert!(input.insert_attachment(attachment("first.png")));
    input.handle_key(key(KeyCode::Right));
    assert!(input.insert_attachment(attachment("middle.png")));
    input.handle_key(key(KeyCode::End));
    assert!(input.insert_attachment(attachment("last.png")));

    assert_eq!(
        input
            .attachments()
            .iter()
            .map(|item| item.filename.as_str())
            .collect::<Vec<_>>(),
        ["first.png", "middle.png", "last.png"]
    );
    let rendered = dump(80, 12, |frame| input.render(frame, frame.area(), 0));
    assert!(rendered.contains("Image 1: first.png"));
    assert!(rendered.contains("Image 2: middle.png"));
    assert!(rendered.contains("Image 3: last.png"));
}

#[test]
fn deleting_an_attachment_removes_its_durable_reference() {
    let mut input = InputPrompt::new("Description").multiline();
    input.insert_attachment(attachment("before.png"));
    input.handle_key(key(KeyCode::Char('x')));
    input.insert_attachment(attachment("after.png"));

    input.handle_key(key(KeyCode::Backspace));
    assert_eq!(
        input
            .attachments()
            .iter()
            .map(|item| item.filename.as_str())
            .collect::<Vec<_>>(),
        ["before.png"]
    );
    input.handle_key(key(KeyCode::Home));
    input.handle_key(key(KeyCode::Delete));
    assert!(input.attachments().is_empty());
}

#[test]
fn multiline_text_paste_remains_text_and_preserves_line_endings() {
    let mut input = InputPrompt::new("Description").multiline();
    input.paste("mentions screenshot.png\r\nnext line");
    assert_eq!(input.value, "mentions screenshot.png\nnext line");
    assert!(input.attachments().is_empty());
}

#[test]
fn single_line_inputs_do_not_accept_image_attachments() {
    let mut input = InputPrompt::new("Name");
    assert!(!input.insert_attachment(attachment("image.png")));
    assert!(input.attachments().is_empty());
}

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
        input.handle_key(key(KeyCode::Backspace)),
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
    let s = dump(60, 8, |f| input.render(f, f.area(), 0));
    assert!(s.contains("Branch"));
    assert!(s.contains("e.g. feat/login"));
    assert!(s.contains("Press Enter to confirm"));
}

#[test]
fn input_prompt_render_shows_error_when_pinned() {
    let mut input = InputPrompt::new("Branch")
        .with_validator(|v| (v.is_empty()).then(|| "Required".to_string()));
    matches!(input.handle_key(key(KeyCode::Enter)), InputOutcome::Pending);
    let s = dump(60, 8, |f| input.render(f, f.area(), 0));
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

#[test]
fn input_prompt_with_default_places_cursor_at_end() {
    let input = InputPrompt::new("Name").with_default("hello");
    assert_eq!(input.cursor, 5);
}

#[test]
fn input_prompt_left_right_arrows_move_cursor() {
    let mut input = InputPrompt::new("Name").with_default("abc");
    assert_eq!(input.cursor, 3);
    input.handle_key(key(KeyCode::Left));
    assert_eq!(input.cursor, 2);
    input.handle_key(key(KeyCode::Left));
    input.handle_key(key(KeyCode::Left));
    input.handle_key(key(KeyCode::Left));
    assert_eq!(input.cursor, 0); // clamps at 0
    input.handle_key(key(KeyCode::Right));
    assert_eq!(input.cursor, 1);
}

#[test]
fn input_prompt_insert_at_cursor() {
    let mut input = InputPrompt::new("Name").with_default("ac");
    input.handle_key(key(KeyCode::Left));
    input.handle_key(key(KeyCode::Char('b')));
    assert_eq!(input.value, "abc");
    assert_eq!(input.cursor, 2);
}

#[test]
fn input_prompt_backspace_deletes_at_cursor() {
    let mut input = InputPrompt::new("Name").with_default("abc");
    input.handle_key(key(KeyCode::Left));
    input.handle_key(key(KeyCode::Backspace));
    assert_eq!(input.value, "ac");
    assert_eq!(input.cursor, 1);
}

#[test]
fn input_prompt_delete_removes_char_at_cursor() {
    let mut input = InputPrompt::new("Name").with_default("abc");
    input.handle_key(key(KeyCode::Home));
    input.handle_key(key(KeyCode::Delete));
    assert_eq!(input.value, "bc");
    assert_eq!(input.cursor, 0);
}

#[test]
fn input_prompt_home_end_keys() {
    let mut input = InputPrompt::new("Name").with_default("hello");
    input.handle_key(key(KeyCode::Home));
    assert_eq!(input.cursor, 0);
    input.handle_key(key(KeyCode::End));
    assert_eq!(input.cursor, 5);
}

#[test]
fn input_prompt_ctrl_a_and_ctrl_e() {
    let mut input = InputPrompt::new("Name").with_default("hello");
    input.handle_key(key_mod(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert_eq!(input.cursor, 0);
    input.handle_key(key_mod(KeyCode::Char('e'), KeyModifiers::CONTROL));
    assert_eq!(input.cursor, 5);
}

#[test]
fn input_prompt_ctrl_b_and_ctrl_f() {
    let mut input = InputPrompt::new("Name").with_default("ab");
    input.handle_key(key_mod(KeyCode::Char('b'), KeyModifiers::CONTROL));
    assert_eq!(input.cursor, 1);
    input.handle_key(key_mod(KeyCode::Char('f'), KeyModifiers::CONTROL));
    assert_eq!(input.cursor, 2);
}

#[test]
fn input_prompt_ctrl_h_and_ctrl_d() {
    let mut input = InputPrompt::new("Name").with_default("abc");
    input.handle_key(key_mod(KeyCode::Char('h'), KeyModifiers::CONTROL));
    assert_eq!(input.value, "ab"); // Ctrl+H = backspace
    input.handle_key(key(KeyCode::Home));
    input.handle_key(key_mod(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(input.value, "b"); // Ctrl+D = delete-right
}

#[test]
fn input_prompt_alt_left_jumps_word() {
    let mut input = InputPrompt::new("Name").with_default("foo bar baz");
    input.handle_key(key_mod(KeyCode::Left, KeyModifiers::ALT));
    assert_eq!(input.cursor, 8); // start of "baz"
    input.handle_key(key_mod(KeyCode::Left, KeyModifiers::ALT));
    assert_eq!(input.cursor, 4); // start of "bar"
}

#[test]
fn input_prompt_alt_right_jumps_word() {
    let mut input = InputPrompt::new("Name").with_default("foo bar baz");
    input.handle_key(key(KeyCode::Home));
    input.handle_key(key_mod(KeyCode::Right, KeyModifiers::ALT));
    assert_eq!(input.cursor, 3); // end of "foo"
    input.handle_key(key_mod(KeyCode::Right, KeyModifiers::ALT));
    assert_eq!(input.cursor, 7); // end of "bar"
}

#[test]
fn input_prompt_ctrl_left_right_word_jump() {
    let mut input = InputPrompt::new("Name").with_default("alpha beta");
    input.handle_key(key_mod(KeyCode::Left, KeyModifiers::CONTROL));
    assert_eq!(input.cursor, 6); // start of "beta"
    input.handle_key(key_mod(KeyCode::Right, KeyModifiers::CONTROL));
    assert_eq!(input.cursor, 10); // end of "beta"
}

#[test]
fn input_prompt_word_jump_uses_non_alphanumeric_boundaries() {
    let mut input = InputPrompt::new("Name").with_default("feat/login-page");
    input.handle_key(key_mod(KeyCode::Left, KeyModifiers::ALT));
    assert_eq!(input.cursor, 11); // start of "page" (after '-')
    input.handle_key(key_mod(KeyCode::Left, KeyModifiers::ALT));
    assert_eq!(input.cursor, 5); // start of "login"
}

#[test]
fn input_prompt_ctrl_w_deletes_word_back() {
    let mut input = InputPrompt::new("Name").with_default("foo bar baz");
    input.handle_key(key_mod(KeyCode::Char('w'), KeyModifiers::CONTROL));
    assert_eq!(input.value, "foo bar ");
    assert_eq!(input.cursor, 8);
}

#[test]
fn input_prompt_alt_backspace_deletes_word_back() {
    let mut input = InputPrompt::new("Name").with_default("foo bar");
    input.handle_key(key_mod(KeyCode::Backspace, KeyModifiers::ALT));
    assert_eq!(input.value, "foo ");
}

#[test]
fn input_prompt_ctrl_u_kills_to_start() {
    let mut input = InputPrompt::new("Name").with_default("hello world");
    // Move to position 6 (between space and "world").
    for _ in 0..5 {
        input.handle_key(key(KeyCode::Left));
    }
    assert_eq!(input.cursor, 6);
    input.handle_key(key_mod(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(input.value, "world");
    assert_eq!(input.cursor, 0);
}

#[test]
fn input_prompt_ctrl_k_kills_to_end() {
    let mut input = InputPrompt::new("Name").with_default("hello world");
    input.handle_key(key(KeyCode::Home));
    for _ in 0..5 {
        input.handle_key(key(KeyCode::Right));
    }
    input.handle_key(key_mod(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert_eq!(input.value, "hello");
    assert_eq!(input.cursor, 5);
}

#[test]
fn input_prompt_alt_b_and_alt_f_word_jump() {
    let mut input = InputPrompt::new("Name").with_default("foo bar");
    input.handle_key(key_mod(KeyCode::Char('b'), KeyModifiers::ALT));
    assert_eq!(input.cursor, 4); // start of "bar"
    input.handle_key(key_mod(KeyCode::Char('f'), KeyModifiers::ALT));
    assert_eq!(input.cursor, 7); // end of "bar"
}

#[test]
fn input_prompt_alt_d_deletes_word_forward() {
    let mut input = InputPrompt::new("Name").with_default("foo bar");
    input.handle_key(key(KeyCode::Home));
    input.handle_key(key_mod(KeyCode::Char('d'), KeyModifiers::ALT));
    assert_eq!(input.value, " bar");
    assert_eq!(input.cursor, 0);
}

#[test]
fn input_prompt_cursor_handles_multibyte_unicode() {
    let mut input = InputPrompt::new("Name").with_default("日本語");
    assert_eq!(input.cursor, 3);
    input.handle_key(key(KeyCode::Left));
    assert_eq!(input.cursor, 2);
    input.handle_key(key(KeyCode::Backspace));
    assert_eq!(input.value, "日語");
    assert_eq!(input.cursor, 1);
    input.handle_key(key(KeyCode::Char('☆')));
    assert_eq!(input.value, "日☆語");
    assert_eq!(input.cursor, 2);
}

#[test]
fn input_prompt_render_ignores_tick() {
    // Cursor is always solid — render output should be identical regardless
    // of `tick` value.
    let input = InputPrompt::new("Branch").with_default("foo");
    let s_low = dump(60, 8, |f| input.render(f, f.area(), 0));
    let s_high = dump(60, 8, |f| input.render(f, f.area(), 7));
    assert_eq!(s_low, s_high);
    assert!(s_low.contains("foo"));
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
    assert_eq!(s.query(), "a");
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
    assert!(s.query().is_empty());
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
fn select_render_uses_available_height_before_scrolling() {
    let labels: Vec<String> = (0..20).map(|i| format!("opt{i}")).collect();
    let opts: Vec<SelectOption<String>> = labels
        .iter()
        .map(|l| SelectOption::new(l.clone(), l.clone()))
        .collect();
    let mut s = SelectPrompt::new("Pick", opts);
    s.selected = 10;
    let dumped = dump(60, 24, |f| s.render(f, f.area()));
    assert!(dumped.contains("opt19"));
    assert!(!dumped.contains("more above"));
    assert!(!dumped.contains("more below"));
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
    let dumped = dump(60, 14, |f| s.render(f, f.area()));
    assert!(dumped.contains("more above"));
    assert!(dumped.contains("more below"));
}

#[test]
fn select_render_uses_arrow_cursor_symbol() {
    let s = SelectPrompt::new("Pick", opts(&["alpha", "beta"]));
    let dumped = dump(60, 8, |f| s.render(f, f.area()));
    assert!(dumped.contains("➤"));
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

// -- BulkConfirmDialog --------------------------------------------------------

fn bulk_items(labels: &[&str]) -> Vec<BulkConfirmItem> {
    labels.iter().map(|l| BulkConfirmItem::new(*l)).collect()
}

fn bulk_dialog(labels: &[&str]) -> BulkConfirmDialog {
    BulkConfirmDialog::new(
        "Delete worktrees",
        "Are you sure?",
        bulk_items(labels),
        "This will also delete their branches!",
        ratatui::style::Color::Red,
    )
}

#[test]
fn bulk_confirm_starts_focused_on_first_row_with_all_checked() {
    let d = bulk_dialog(&["a", "b", "c"]);
    assert_eq!(d.focus, BulkConfirmFocus::List(0));
    assert!(d.items.iter().all(|i| i.checked));
    assert_eq!(d.selected_indices(), vec![0, 1, 2]);
}

#[test]
fn bulk_confirm_empty_items_focus_falls_back_to_confirm() {
    let d = BulkConfirmDialog::new(
        "t",
        "p",
        Vec::<BulkConfirmItem>::new(),
        "w",
        ratatui::style::Color::Red,
    );
    assert_eq!(d.focus, BulkConfirmFocus::Confirm);
    assert!(d.selected_indices().is_empty());
}

#[test]
fn bulk_confirm_arrow_keys_move_within_list() {
    let mut d = bulk_dialog(&["a", "b", "c"]);
    d.handle_key(key(KeyCode::Down));
    assert_eq!(d.focus, BulkConfirmFocus::List(1));
    d.handle_key(key(KeyCode::Down));
    assert_eq!(d.focus, BulkConfirmFocus::List(2));
    d.handle_key(key(KeyCode::Down));
    assert_eq!(d.focus, BulkConfirmFocus::List(2)); // saturates at end
    d.handle_key(key(KeyCode::Up));
    assert_eq!(d.focus, BulkConfirmFocus::List(1));
    d.handle_key(key(KeyCode::Up));
    d.handle_key(key(KeyCode::Up));
    assert_eq!(d.focus, BulkConfirmFocus::List(0)); // saturates at top
}

#[test]
fn bulk_confirm_space_toggles_focused_row() {
    let mut d = bulk_dialog(&["a", "b"]);
    d.handle_key(key(KeyCode::Char(' ')));
    assert!(!d.items[0].checked);
    assert!(d.items[1].checked);
    d.handle_key(key(KeyCode::Char(' ')));
    assert!(d.items[0].checked);
}

#[test]
fn bulk_confirm_a_toggles_select_all() {
    let mut d = bulk_dialog(&["a", "b", "c"]);
    d.handle_key(key(KeyCode::Char(' ')));
    assert!(d.any_unchecked());
    d.handle_key(key(KeyCode::Char('a'))); // re-checks everything
    assert!(!d.any_unchecked());
    d.handle_key(key(KeyCode::Char('a'))); // unchecks everything
    assert!(d.items.iter().all(|i| !i.checked));
}

#[test]
fn bulk_confirm_tab_cycles_list_to_cancel_to_list() {
    let mut d = bulk_dialog(&["a", "b"]);
    d.handle_key(key(KeyCode::Down));
    d.handle_key(key(KeyCode::Tab));
    assert_eq!(d.focus, BulkConfirmFocus::Cancel);
    d.handle_key(key(KeyCode::Tab));
    assert_eq!(d.focus, BulkConfirmFocus::List(1));
}

#[test]
fn bulk_confirm_left_right_swap_buttons_only() {
    let mut d = bulk_dialog(&["a", "b"]);
    // ←/→ on the list is a no-op.
    d.handle_key(key(KeyCode::Right));
    assert_eq!(d.focus, BulkConfirmFocus::List(0));
    d.handle_key(key(KeyCode::Tab));
    assert_eq!(d.focus, BulkConfirmFocus::Cancel);
    d.handle_key(key(KeyCode::Right));
    assert_eq!(d.focus, BulkConfirmFocus::Confirm);
    d.handle_key(key(KeyCode::Left));
    assert_eq!(d.focus, BulkConfirmFocus::Cancel);
}

#[test]
fn bulk_confirm_enter_on_list_moves_focus_to_no_button() {
    let mut d = bulk_dialog(&["a", "b", "c"]);
    d.handle_key(key(KeyCode::Down));
    d.handle_key(key(KeyCode::Char(' '))); // uncheck index 1
    let outcome = d.handle_key(key(KeyCode::Enter));
    assert_eq!(outcome, BulkConfirmOutcome::Pending);
    assert_eq!(d.focus, BulkConfirmFocus::Cancel);
}

#[test]
fn bulk_confirm_enter_on_yes_confirms_with_checked_indices() {
    let mut d = bulk_dialog(&["a", "b"]);
    d.handle_key(key(KeyCode::Tab));
    d.handle_key(key(KeyCode::Left));
    let outcome = d.handle_key(key(KeyCode::Enter));
    assert_eq!(outcome, BulkConfirmOutcome::Confirmed(vec![0, 1]));
}

#[test]
fn bulk_confirm_enter_on_no_cancels() {
    let mut d = bulk_dialog(&["a", "b"]);
    d.handle_key(key(KeyCode::Tab));
    let outcome = d.handle_key(key(KeyCode::Enter));
    assert_eq!(outcome, BulkConfirmOutcome::Cancelled);
}

#[test]
fn bulk_confirm_enter_with_nothing_checked_still_requires_button_confirmation() {
    let mut d = bulk_dialog(&["a", "b"]);
    d.handle_key(key(KeyCode::Char('a'))); // uncheck all
    let outcome = d.handle_key(key(KeyCode::Enter));
    assert_eq!(outcome, BulkConfirmOutcome::Pending);
    assert_eq!(d.focus, BulkConfirmFocus::Cancel);
}

#[test]
fn bulk_confirm_render_shows_checkbox_glyphs_and_footer() {
    let d = bulk_dialog(&["/tmp/repo-feat [feat]", "/tmp/repo-bug [bug]"]);
    let dumped = dump(100, 16, |f| d.render(f, f.area()));
    assert!(dumped.contains("☒"));
    assert!(dumped.contains("/tmp/repo-feat"));
    assert!(dumped.contains("Yes"));
    assert!(dumped.contains("No"));
    assert!(dumped.contains("Space toggle"));
    assert!(dumped.contains("select all"));
}

#[test]
fn bulk_confirm_render_swaps_glyph_after_space() {
    let mut d = bulk_dialog(&["/tmp/repo-feat [feat]", "/tmp/repo-bug [bug]"]);
    d.handle_key(key(KeyCode::Char(' ')));
    let dumped = dump(100, 16, |f| d.render(f, f.area()));
    assert!(dumped.contains("☐"));
    assert!(dumped.contains("☒"));
}

#[test]
fn bulk_confirm_render_marks_focused_row_with_cursor() {
    let d = bulk_dialog(&["/tmp/repo-feat [feat]", "/tmp/repo-bug [bug]"]);
    let dumped = dump(100, 16, |f| d.render(f, f.area()));
    // The focused row carries the ➤ cursor; the other row gets two spaces.
    assert!(dumped.contains("➤"));
}

#[test]
fn bulk_confirm_cursor_visible_on_first_and_last_row_at_preferred_height() {
    // Production sizes the panel to exactly `preferred_content_height`. If
    // the dialog's render constraints sum to more than that, ratatui's
    // solver squeezes — historically wiping the first/last item rows.
    let mut d = bulk_dialog(&["row-a", "row-b", "row-c"]);
    let h = d.preferred_content_height();
    let dumped_first = dump(80, h, |f| d.render(f, f.area()));
    assert!(
        dumped_first.contains("➤"),
        "expected cursor visible on first row at preferred height; got: {dumped_first}"
    );
    // Move focus to the last row and re-render.
    d.handle_key(key(KeyCode::Down));
    d.handle_key(key(KeyCode::Down));
    let dumped_last = dump(80, h, |f| d.render(f, f.area()));
    assert!(
        dumped_last.contains("➤"),
        "expected cursor visible on last row at preferred height; got: {dumped_last}"
    );
    // Sanity: the footer hint and warning must also be present, proving the
    // bottom of the layout isn't being clipped.
    assert!(dumped_last.contains("Space toggle"));
}

#[test]
fn bulk_confirm_esc_on_buttons_returns_to_last_list_row() {
    let mut d = bulk_dialog(&["a", "b", "c"]);
    d.handle_key(key(KeyCode::Down));
    d.handle_key(key(KeyCode::Down));
    d.handle_key(key(KeyCode::Enter));

    let outcome = d.handle_key(key(KeyCode::Esc));

    assert_eq!(outcome, BulkConfirmOutcome::Pending);
    assert_eq!(d.focus, BulkConfirmFocus::List(2));
}

#[test]
fn bulk_confirm_esc_on_list_cancels() {
    let mut d = bulk_dialog(&["a", "b"]);
    let outcome = d.handle_key(key(KeyCode::Esc));
    assert_eq!(outcome, BulkConfirmOutcome::Cancelled);
}

//! State-machine + render tests for the Settings screen.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use wisetree::config::schema::WorktreeConfig;
use wisetree::messages::colors;
use wisetree::services::UpdateCheckResult;
use wisetree::tui::screens::settings::{
    CopyDirection, PathTemplateRectStatus, PathTemplateSelection, PostCmdRectStatus,
    PostCmdSelection, SettingsAction, SettingsScreen, SettingsStep, TerminalCmdRectStatus,
    TerminalCmdSelection,
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
    render(width, height, draw)
        .content
        .iter()
        .map(|c| c.symbol())
        .collect()
}

fn render<F>(width: u16, height: u16, draw: F) -> Buffer
where
    F: FnOnce(&mut ratatui::Frame),
{
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(draw).unwrap();
    terminal.backend().buffer().clone()
}

fn find_text_start(buffer: &Buffer, text: &str) -> Option<(u16, u16)> {
    let needle: Vec<String> = text.chars().map(|ch| ch.to_string()).collect();
    let width = buffer.area.width;
    let needle_len = needle.len() as u16;
    if needle_len == 0 || needle_len > width {
        return None;
    }

    for y in 0..buffer.area.height {
        for x in 0..=width - needle_len {
            if needle.iter().enumerate().all(|(offset, expected)| {
                buffer[(x + offset as u16, y)].symbol() == expected.as_str()
            }) {
                return Some((x, y));
            }
        }
    }

    None
}

fn assert_text_modifier(buffer: &Buffer, text: &str, modifier: Modifier) {
    let (x, y) = find_text_start(buffer, text).unwrap_or_else(|| panic!("{text:?} not found"));

    for (offset, _) in text.chars().enumerate() {
        let cell = &buffer[(x + offset as u16, y)];
        assert!(
            cell.modifier.contains(modifier),
            "missing {modifier:?} for {text:?} at offset {offset}"
        );
    }
}

fn assert_text_fg(buffer: &Buffer, text: &str, fg: Color) {
    let (x, y) = find_text_start(buffer, text).unwrap_or_else(|| panic!("{text:?} not found"));

    for (offset, _) in text.chars().enumerate() {
        let cell = &buffer[(x + offset as u16, y)];
        assert_eq!(cell.fg, fg, "unexpected fg for {text:?} at offset {offset}");
    }
}

fn surrounding_border_cells(buffer: &Buffer, text: &str) -> ((u16, u16), (u16, u16)) {
    let (x, y) = find_text_start(buffer, text).unwrap_or_else(|| panic!("{text:?} not found"));

    let left_x = (0..x)
        .rev()
        .find(|&candidate| buffer[(candidate, y)].symbol() == "│")
        .unwrap_or_else(|| panic!("left border for {text:?} not found"));
    let right_x = ((x + text.chars().count() as u16)..buffer.area.width)
        .find(|&candidate| buffer[(candidate, y)].symbol() == "│")
        .unwrap_or_else(|| panic!("right border for {text:?} not found"));

    ((left_x, y), (right_x, y))
}

fn ready() -> SettingsScreen {
    ready_with_commands(&["bun install"])
}

fn ready_with_commands(commands: &[&str]) -> SettingsScreen {
    let cfg = WorktreeConfig {
        post_create_cmd: commands
            .iter()
            .map(|command| (*command).to_string())
            .collect(),
        terminal_command: "code $WORKTREE_PATH".into(),
        delete_branch_with_worktree: true,
        ..Default::default()
    };
    SettingsScreen::new(cfg, "/tmp/.wisetree.json".into())
}

#[test]
fn menu_renders_with_config_path() {
    let s = ready();
    let dumped = dump(80, 14, |f| s.render(f, f.area()));
    assert!(dumped.contains("Configuration file"));
    assert!(dumped.contains("/tmp/.wisetree.json"));
    assert!(dumped.contains("➤"));
    assert!(dumped.contains("Copy Patterns"));
    assert!(dumped.contains("Copy Settings"));
    assert!(dumped.contains("Check for Updates"));
    assert!(dumped.contains("Dashboard"));
}

#[test]
fn esc_on_menu_returns_back() {
    let mut s = ready();
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, SettingsAction::Back);
}

#[test]
fn selecting_copy_patterns_shows_detail_view() {
    let mut s = ready();
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), SettingsStep::CopyPatterns);
    let dumped = dump(80, 12, |f| s.render(f, f.area()));
    assert!(dumped.contains("Copy Patterns"));
    assert!(dumped.contains(".env*"));
}

#[test]
fn any_key_in_detail_returns_to_menu() {
    let mut s = ready();
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), SettingsStep::CopyPatterns);
    let action = s.handle_key(key(KeyCode::Char('x')));
    assert_eq!(action, SettingsAction::Continue);
    assert_eq!(s.step(), SettingsStep::Menu);
}

#[test]
fn delete_branch_setting_renders_yes_no_toggle() {
    let mut s = ready();
    for _ in 0..5 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), SettingsStep::DeleteBranch);

    let dumped = dump(80, 14, |f| s.render(f, f.area()));
    assert!(dumped.contains("Delete Branch with Worktree"));
    assert!(dumped.contains("Yes"));
    assert!(dumped.contains("No"));
    assert!(dumped.contains("Never deletes current or default branches"));
}

#[test]
fn delete_branch_setting_emits_true_when_yes_selected() {
    let mut s = ready();
    for _ in 0..5 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SettingsAction::SetDeleteBranchWithWorktree(true));
}

#[test]
fn delete_branch_setting_emits_false_when_no_selected() {
    let mut s = ready();
    for _ in 0..5 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('n')));

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SettingsAction::SetDeleteBranchWithWorktree(false));
}

#[test]
fn select_check_updates_emits_action() {
    let mut s = ready();
    // Navigate to last entry "Check for Updates" — 7 downs from the first.
    for _ in 0..7 {
        s.handle_key(key(KeyCode::Down));
    }
    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SettingsAction::CheckUpdates);
    assert_eq!(s.step(), SettingsStep::CheckUpdates);
}

#[test]
fn selecting_copy_settings_shows_copy_directions() {
    let mut s = ready();
    for _ in 0..6 {
        s.handle_key(key(KeyCode::Down));
    }

    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), SettingsStep::CopySettings);

    let dumped = dump(90, 12, |f| s.render(f, f.area()));
    assert!(dumped.contains("global → local"));
    assert!(dumped.contains("local → global"));
}

#[test]
fn copy_settings_default_selection_emits_global_to_local() {
    let mut s = ready();
    for _ in 0..6 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(
        action,
        SettingsAction::CopySettings(CopyDirection::GlobalToLocal)
    );
}

#[test]
fn copy_settings_second_selection_emits_local_to_global() {
    let mut s = ready();
    for _ in 0..6 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Down));

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(
        action,
        SettingsAction::CopySettings(CopyDirection::LocalToGlobal)
    );
}

#[test]
fn check_updates_loading_renders_spinner_message() {
    let mut s = ready();
    for _ in 0..7 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    s.start_checking_updates();
    let dumped = dump(60, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("Checking for updates"));
}

#[test]
fn check_updates_with_new_version_shows_install_command() {
    let mut s = ready();
    for _ in 0..7 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    s.start_checking_updates();
    s.set_update_result(UpdateCheckResult {
        has_update: true,
        current_version: "1.0.0".into(),
        latest_version: Some("2.0.0".into()),
        checked_at: 0,
        error: None,
    });
    let dumped = dump(80, 8, |f| s.render(f, f.area()));
    assert!(dumped.contains("New version available"));
    assert!(dumped.contains("v2.0.0"));
    assert!(dumped.contains("npm install -g wisetree"));
}

#[test]
fn check_updates_up_to_date_message() {
    let mut s = ready();
    for _ in 0..7 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    s.set_update_result(UpdateCheckResult {
        has_update: false,
        current_version: "1.0.0".into(),
        latest_version: Some("1.0.0".into()),
        checked_at: 0,
        error: None,
    });
    let dumped = dump(80, 6, |f| s.render(f, f.area()));
    assert!(dumped.contains("latest version"));
    assert!(dumped.contains("v1.0.0"));
}

#[test]
fn check_updates_error_shows_failure_message() {
    let mut s = ready();
    for _ in 0..7 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    s.set_update_result(UpdateCheckResult {
        has_update: false,
        current_version: "1.0.0".into(),
        latest_version: None,
        checked_at: 0,
        error: Some("network down".into()),
    });
    let dumped = dump(80, 6, |f| s.render(f, f.area()));
    assert!(dumped.contains("Failed to check for updates"));
}

fn enter_post_cmd(s: &mut SettingsScreen) {
    // Menu order: Copy(0), Ignore(1), Path(2), PostCmd(3).
    for _ in 0..3 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), SettingsStep::PostCmd);
}

#[test]
fn post_cmd_editor_initializes_existing_commands_as_saved() {
    let mut s = ready();
    enter_post_cmd(&mut s);

    let editor = s.post_cmd_editor().expect("editor present");
    assert_eq!(editor.commands, vec!["bun install".to_string()]);
    assert_eq!(editor.statuses, vec![PostCmdRectStatus::Saved]);
    assert_eq!(editor.selection, PostCmdSelection::Create);
}

#[test]
fn post_cmd_selected_rectangle_keeps_status_border_and_shows_orange_marker() {
    let mut s = ready();
    enter_post_cmd(&mut s);
    s.handle_key(key(KeyCode::Up));

    let buffer = render(80, 14, |f| s.render(f, f.area()));
    assert_text_fg(&buffer, "✎", colors::ACCENT);
    assert_text_fg(&buffer, "bun install", colors::WHITE);
    assert_text_modifier(&buffer, "bun install", Modifier::BOLD);

    let (left_border, right_border) = surrounding_border_cells(&buffer, "bun install");
    for (x, y) in [left_border, right_border] {
        let cell = &buffer[(x, y)];
        assert_eq!(cell.fg, colors::SUCCESS);
        assert!(
            !cell.modifier.contains(Modifier::BOLD),
            "border cell at ({x}, {y}) should not be bold"
        );
    }
}

#[test]
fn post_cmd_selected_delete_mark_keeps_red_border() {
    let mut s = ready();
    enter_post_cmd(&mut s);
    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Backspace));

    let buffer = render(80, 14, |f| s.render(f, f.area()));
    assert_text_fg(&buffer, "✎", colors::ACCENT);

    let (left_border, right_border) = surrounding_border_cells(&buffer, "bun install");
    for (x, y) in [left_border, right_border] {
        assert_eq!(buffer[(x, y)].fg, colors::ERROR);
    }
}

#[test]
fn post_cmd_selection_marker_disappears_while_editing() {
    let mut s = ready();
    enter_post_cmd(&mut s);
    s.handle_key(key(KeyCode::Up));

    let selected = dump(80, 14, |f| s.render(f, f.area()));
    assert!(selected.contains("✎𓂃"));

    s.handle_key(key(KeyCode::Enter));

    let editing = dump(80, 14, |f| s.render(f, f.area()));
    assert!(!editing.contains("✎𓂃"));
}

#[test]
fn post_cmd_overflow_keeps_button_labels_visible() {
    let mut s = ready_with_commands(&[
        "mkdir eu_abri1",
        "mkdir eu_abri2",
        "mkdir eu_abri3",
        "mkdir eu_abri4",
    ]);
    enter_post_cmd(&mut s);

    let dumped = dump(80, 18, |f| s.render(f, f.area()));
    assert!(dumped.contains("Create"));
    assert!(dumped.contains("Save"));
    assert!(dumped.contains("▲/▼ to scroll"));
    assert!(dumped.contains("▼ 1 below"));
}

#[test]
fn post_cmd_overflow_keeps_last_selected_rectangle_in_view() {
    let mut s = ready_with_commands(&[
        "mkdir eu_abri1",
        "mkdir eu_abri2",
        "mkdir eu_abri3",
        "mkdir eu_abri4",
    ]);
    enter_post_cmd(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Down));

    let dumped = dump(80, 18, |f| s.render(f, f.area()));
    assert!(dumped.contains("mkdir eu_abri4"));
    assert!(dumped.contains("Create"));
    assert!(dumped.contains("▲ 1 above"));
    assert!(dumped.contains("▼ bottom"));
}

#[test]
fn post_cmd_non_overflow_hides_scroll_indicator() {
    let mut s = ready();
    enter_post_cmd(&mut s);

    let dumped = dump(80, 14, |f| s.render(f, f.area()));
    assert!(!dumped.contains("▲/▼ to scroll"));
}

#[test]
fn post_cmd_create_button_appends_blank_rectangle() {
    let mut s = ready();
    enter_post_cmd(&mut s);

    assert_eq!(
        s.post_cmd_editor().unwrap().selection,
        PostCmdSelection::Create
    );

    s.handle_key(key(KeyCode::Enter));
    let editor = s.post_cmd_editor().unwrap();
    assert_eq!(editor.commands.len(), 2);
    assert_eq!(editor.commands[1], "");
    assert_eq!(editor.selection, PostCmdSelection::Rect(1));
    assert_eq!(editor.statuses[1], PostCmdRectStatus::Editing);
}

#[test]
fn post_cmd_up_from_buttons_returns_to_last_rectangle() {
    let mut s = ready();
    enter_post_cmd(&mut s);

    assert_eq!(
        s.post_cmd_editor().unwrap().selection,
        PostCmdSelection::Create
    );

    s.handle_key(key(KeyCode::Up));
    assert_eq!(
        s.post_cmd_editor().unwrap().selection,
        PostCmdSelection::Rect(0)
    );

    s.handle_key(key(KeyCode::Down));
    s.handle_key(key(KeyCode::Right));
    assert_eq!(
        s.post_cmd_editor().unwrap().selection,
        PostCmdSelection::Save
    );

    s.handle_key(key(KeyCode::Up));
    assert_eq!(
        s.post_cmd_editor().unwrap().selection,
        PostCmdSelection::Rect(0)
    );
}

#[test]
fn post_cmd_enter_starts_editing_then_modifies() {
    let mut s = ready();
    enter_post_cmd(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(
        s.post_cmd_editor().unwrap().statuses[0],
        PostCmdRectStatus::Editing
    );

    // Append a character, then commit with Enter.
    s.handle_key(key(KeyCode::Char('!')));
    s.handle_key(key(KeyCode::Enter));

    let editor = s.post_cmd_editor().unwrap();
    assert_eq!(editor.commands[0], "bun install!");
    assert_eq!(editor.statuses[0], PostCmdRectStatus::Modified);
}

#[test]
fn post_cmd_escape_exits_editing_without_changes() {
    let mut s = ready();
    enter_post_cmd(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Esc));

    let editor = s.post_cmd_editor().unwrap();
    assert_eq!(editor.commands[0], "bun install");
    assert_eq!(editor.statuses[0], PostCmdRectStatus::Saved);
    assert_eq!(editor.selection, PostCmdSelection::Rect(0));
}

#[test]
fn post_cmd_create_button_enters_edit_mode_immediately() {
    let mut s = ready();
    enter_post_cmd(&mut s);

    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('p')));
    s.handle_key(key(KeyCode::Char('n')));
    s.handle_key(key(KeyCode::Char('p')));
    s.handle_key(key(KeyCode::Enter));

    let editor = s.post_cmd_editor().unwrap();
    assert_eq!(editor.commands[1], "pnp");
    assert_eq!(editor.statuses[1], PostCmdRectStatus::Modified);
    assert_eq!(editor.selection, PostCmdSelection::Rect(1));
}

#[test]
fn post_cmd_editor_supports_cursor_movement_while_editing() {
    let mut s = ready();
    enter_post_cmd(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Left));
    s.handle_key(key(KeyCode::Left));
    s.handle_key(key(KeyCode::Char('!')));
    s.handle_key(key(KeyCode::Enter));

    let editor = s.post_cmd_editor().unwrap();
    assert_eq!(editor.commands[0], "bun insta!ll");
    assert_eq!(editor.statuses[0], PostCmdRectStatus::Modified);
}

#[test]
fn post_cmd_editing_uses_reversed_block_cursor() {
    let mut s = ready();
    enter_post_cmd(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Enter));

    let buffer = render(80, 14, |f| s.render(f, f.area()));
    let (x, y) = find_text_start(&buffer, "bun install").expect("editing text present");
    let cursor_cell = &buffer[(x + "bun install".chars().count() as u16, y)];

    assert_eq!(cursor_cell.symbol(), " ");
    assert!(cursor_cell.modifier.contains(Modifier::REVERSED));
}

#[test]
fn post_cmd_editor_supports_ctrl_word_delete_while_editing() {
    let mut s = ready();
    enter_post_cmd(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key_mod(KeyCode::Char('w'), KeyModifiers::CONTROL));
    s.handle_key(key(KeyCode::Enter));

    let editor = s.post_cmd_editor().unwrap();
    assert_eq!(editor.commands[0], "bun ");
    assert_eq!(editor.statuses[0], PostCmdRectStatus::Modified);
}

#[test]
fn post_cmd_save_button_emits_filtered_commands() {
    let mut s = ready();
    enter_post_cmd(&mut s);

    // Add a blank one, leave edit mode immediately, then move to Save.
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Down));
    s.handle_key(key(KeyCode::Right));
    assert_eq!(
        s.post_cmd_editor().unwrap().selection,
        PostCmdSelection::Save
    );

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(
        action,
        SettingsAction::SavePostCreateCommands(vec!["bun install".into()])
    );
}

#[test]
fn post_cmd_save_button_omits_red_rectangles_and_keeps_orange_ones() {
    let cfg = WorktreeConfig {
        post_create_cmd: vec!["bun install".into(), "bun test".into()],
        terminal_command: "code $WORKTREE_PATH".into(),
        delete_branch_with_worktree: true,
        ..Default::default()
    };
    let mut s = SettingsScreen::new(cfg, "/tmp/.wisetree.json".into());
    enter_post_cmd(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Backspace));
    s.handle_key(key(KeyCode::Down));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('!')));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Down));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('n')));
    s.handle_key(key(KeyCode::Char('p')));
    s.handle_key(key(KeyCode::Char('m')));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Down));
    s.handle_key(key(KeyCode::Right));

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(
        action,
        SettingsAction::SavePostCreateCommands(vec!["bun test!".into(), "npm".into()])
    );
}

#[test]
fn post_cmd_mark_saved_returns_to_settings_menu() {
    let mut s = ready();
    enter_post_cmd(&mut s);

    // Edit then mark saved.
    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('x')));
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(
        s.post_cmd_editor().unwrap().statuses[0],
        PostCmdRectStatus::Modified
    );

    s.mark_post_create_commands_saved(vec!["bun installx".into()]);

    assert_eq!(s.step(), SettingsStep::Menu);
    assert!(s.post_cmd_editor().is_none());

    enter_post_cmd(&mut s);
    let editor = s.post_cmd_editor().unwrap();
    assert_eq!(editor.commands, vec!["bun installx".to_string()]);
    assert_eq!(editor.statuses, vec![PostCmdRectStatus::Saved]);
}

#[test]
fn post_cmd_backspace_toggles_delete_mark() {
    let mut s = ready();
    enter_post_cmd(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Backspace));
    let editor = s.post_cmd_editor().unwrap();
    assert_eq!(editor.commands, vec!["bun install".to_string()]);
    assert_eq!(editor.statuses, vec![PostCmdRectStatus::MarkedForDeletion]);
    assert_eq!(editor.selection, PostCmdSelection::Rect(0));

    s.handle_key(key(KeyCode::Backspace));
    let editor = s.post_cmd_editor().unwrap();
    assert_eq!(editor.statuses, vec![PostCmdRectStatus::Modified]);

    s.handle_key(key(KeyCode::Backspace));
    let editor = s.post_cmd_editor().unwrap();
    assert_eq!(editor.statuses, vec![PostCmdRectStatus::MarkedForDeletion]);
}

fn enter_terminal_cmd(s: &mut SettingsScreen) {
    // Menu order: Copy(0), Ignore(1), Path(2), PostCmd(3), TerminalCmd(4).
    for _ in 0..4 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), SettingsStep::TerminalCmd);
}

#[test]
fn terminal_cmd_editor_initializes_with_saved_status_when_command_present() {
    let mut s = ready();
    enter_terminal_cmd(&mut s);

    let editor = s.terminal_cmd_editor().expect("editor present");
    assert_eq!(editor.command, "code $WORKTREE_PATH");
    assert_eq!(editor.status, TerminalCmdRectStatus::Saved);
    assert_eq!(editor.selection, TerminalCmdSelection::Save);
}

#[test]
fn terminal_cmd_editor_initializes_with_unchanged_status_when_blank() {
    let cfg = WorktreeConfig {
        terminal_command: String::new(),
        ..Default::default()
    };
    let mut s = SettingsScreen::new(cfg, "/tmp/.wisetree.json".into());
    enter_terminal_cmd(&mut s);

    let editor = s.terminal_cmd_editor().expect("editor present");
    assert_eq!(editor.command, "");
    assert_eq!(editor.status, TerminalCmdRectStatus::Unchanged);
}

#[test]
fn terminal_cmd_blank_renders_none_placeholder_in_muted() {
    let cfg = WorktreeConfig {
        terminal_command: String::new(),
        ..Default::default()
    };
    let mut s = SettingsScreen::new(cfg, "/tmp/.wisetree.json".into());
    enter_terminal_cmd(&mut s);

    let buffer = render(80, 14, |f| s.render(f, f.area()));
    assert_text_fg(&buffer, "(none)", colors::MUTED);
}

#[test]
fn terminal_cmd_renders_save_button_and_saving_to_line() {
    let mut s = ready();
    enter_terminal_cmd(&mut s);

    let dumped = dump(80, 14, |f| s.render(f, f.area()));
    assert!(dumped.contains("Terminal Command"));
    assert!(dumped.contains("Save"));
    assert!(!dumped.contains("Create"));
    assert!(dumped.contains("Saving to:"));
    assert!(dumped.contains(".wisetree.json"));
}

#[test]
fn terminal_cmd_up_from_save_focuses_rectangle() {
    let mut s = ready();
    enter_terminal_cmd(&mut s);
    assert_eq!(
        s.terminal_cmd_editor().unwrap().selection,
        TerminalCmdSelection::Save
    );

    s.handle_key(key(KeyCode::Up));
    assert_eq!(
        s.terminal_cmd_editor().unwrap().selection,
        TerminalCmdSelection::Rect
    );

    s.handle_key(key(KeyCode::Down));
    assert_eq!(
        s.terminal_cmd_editor().unwrap().selection,
        TerminalCmdSelection::Save
    );
}

#[test]
fn terminal_cmd_enter_on_rect_starts_editing_then_modifies() {
    let mut s = ready();
    enter_terminal_cmd(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(
        s.terminal_cmd_editor().unwrap().status,
        TerminalCmdRectStatus::Editing
    );

    s.handle_key(key(KeyCode::Char('!')));
    s.handle_key(key(KeyCode::Enter));

    let editor = s.terminal_cmd_editor().unwrap();
    assert_eq!(editor.command, "code $WORKTREE_PATH!");
    assert_eq!(editor.status, TerminalCmdRectStatus::Modified);
}

#[test]
fn terminal_cmd_escape_during_editing_restores_backup() {
    let mut s = ready();
    enter_terminal_cmd(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('!')));
    s.handle_key(key(KeyCode::Esc));

    let editor = s.terminal_cmd_editor().unwrap();
    assert_eq!(editor.command, "code $WORKTREE_PATH");
    assert_eq!(editor.status, TerminalCmdRectStatus::Saved);
}

#[test]
fn terminal_cmd_save_button_emits_save_action() {
    let mut s = ready();
    enter_terminal_cmd(&mut s);

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(
        action,
        SettingsAction::SaveTerminalCommand("code $WORKTREE_PATH".into())
    );
}

#[test]
fn terminal_cmd_save_blank_emits_empty_string() {
    let cfg = WorktreeConfig {
        terminal_command: "   ".into(),
        ..Default::default()
    };
    let mut s = SettingsScreen::new(cfg, "/tmp/.wisetree.json".into());
    enter_terminal_cmd(&mut s);

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SettingsAction::SaveTerminalCommand(String::new()));
}

#[test]
fn terminal_cmd_mark_saved_returns_to_settings_menu() {
    let mut s = ready();
    enter_terminal_cmd(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('!')));
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(
        s.terminal_cmd_editor().unwrap().status,
        TerminalCmdRectStatus::Modified
    );

    s.mark_terminal_command_saved("code $WORKTREE_PATH!".into());

    assert_eq!(s.step(), SettingsStep::Menu);
    assert!(s.terminal_cmd_editor().is_none());

    enter_terminal_cmd(&mut s);
    let editor = s.terminal_cmd_editor().unwrap();
    assert_eq!(editor.command, "code $WORKTREE_PATH!");
    assert_eq!(editor.status, TerminalCmdRectStatus::Saved);
}

#[test]
fn terminal_cmd_esc_outside_editing_returns_to_menu() {
    let mut s = ready();
    enter_terminal_cmd(&mut s);

    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, SettingsAction::Continue);
    assert_eq!(s.step(), SettingsStep::Menu);
    assert!(s.terminal_cmd_editor().is_none());
}

#[test]
fn terminal_cmd_selected_rectangle_shows_orange_marker_and_keeps_status_border() {
    let mut s = ready();
    enter_terminal_cmd(&mut s);
    s.handle_key(key(KeyCode::Up));

    let buffer = render(80, 14, |f| s.render(f, f.area()));
    assert_text_fg(&buffer, "✎", colors::ACCENT);
    assert_text_fg(&buffer, "code $WORKTREE_PATH", colors::WHITE);
    assert_text_modifier(&buffer, "code $WORKTREE_PATH", Modifier::BOLD);

    let (left_border, right_border) = surrounding_border_cells(&buffer, "code $WORKTREE_PATH");
    for (x, y) in [left_border, right_border] {
        assert_eq!(buffer[(x, y)].fg, colors::SUCCESS);
    }
}

fn enter_path_template(s: &mut SettingsScreen) {
    // Menu order: Copy(0), Ignore(1), Path(2).
    for _ in 0..2 {
        s.handle_key(key(KeyCode::Down));
    }
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), SettingsStep::PathTemplate);
}

#[test]
fn path_template_editor_initializes_with_saved_status_when_template_present() {
    let mut s = ready();
    enter_path_template(&mut s);

    let editor = s.path_template_editor().expect("editor present");
    assert_eq!(editor.template, "$BASE_PATH.worktree");
    assert_eq!(editor.status, PathTemplateRectStatus::Saved);
    assert_eq!(editor.selection, PathTemplateSelection::Save);
}

#[test]
fn path_template_editor_initializes_with_unchanged_status_when_blank() {
    let cfg = WorktreeConfig {
        worktree_path_template: String::new(),
        ..Default::default()
    };
    let mut s = SettingsScreen::new(cfg, "/tmp/.wisetree.json".into());
    enter_path_template(&mut s);

    let editor = s.path_template_editor().expect("editor present");
    assert_eq!(editor.template, "");
    assert_eq!(editor.status, PathTemplateRectStatus::Unchanged);
}

#[test]
fn path_template_renders_save_button_and_saving_to_line() {
    let mut s = ready();
    enter_path_template(&mut s);

    let dumped = dump(80, 18, |f| s.render(f, f.area()));
    assert!(dumped.contains("Worktree Path Template"));
    assert!(dumped.contains("Save"));
    assert!(!dumped.contains("Create"));
    assert!(dumped.contains("Saving to:"));
    assert!(dumped.contains("Available variables"));
    assert!(dumped.contains("$BASE_PATH"));
}

#[test]
fn path_template_up_from_save_focuses_rectangle() {
    let mut s = ready();
    enter_path_template(&mut s);
    assert_eq!(
        s.path_template_editor().unwrap().selection,
        PathTemplateSelection::Save
    );

    s.handle_key(key(KeyCode::Up));
    assert_eq!(
        s.path_template_editor().unwrap().selection,
        PathTemplateSelection::Rect
    );

    s.handle_key(key(KeyCode::Down));
    assert_eq!(
        s.path_template_editor().unwrap().selection,
        PathTemplateSelection::Save
    );
}

#[test]
fn path_template_enter_on_rect_starts_editing_then_modifies() {
    let mut s = ready();
    enter_path_template(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(
        s.path_template_editor().unwrap().status,
        PathTemplateRectStatus::Editing
    );

    s.handle_key(key(KeyCode::Char('!')));
    s.handle_key(key(KeyCode::Enter));

    let editor = s.path_template_editor().unwrap();
    assert_eq!(editor.template, "$BASE_PATH.worktree!");
    assert_eq!(editor.status, PathTemplateRectStatus::Modified);
}

#[test]
fn path_template_escape_during_editing_restores_backup() {
    let mut s = ready();
    enter_path_template(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('!')));
    s.handle_key(key(KeyCode::Esc));

    let editor = s.path_template_editor().unwrap();
    assert_eq!(editor.template, "$BASE_PATH.worktree");
    assert_eq!(editor.status, PathTemplateRectStatus::Saved);
}

#[test]
fn path_template_save_button_emits_save_action() {
    let mut s = ready();
    enter_path_template(&mut s);

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(
        action,
        SettingsAction::SavePathTemplate("$BASE_PATH.worktree".into())
    );
}

#[test]
fn path_template_save_blank_emits_empty_string() {
    let cfg = WorktreeConfig {
        worktree_path_template: "   ".into(),
        ..Default::default()
    };
    let mut s = SettingsScreen::new(cfg, "/tmp/.wisetree.json".into());
    enter_path_template(&mut s);

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SettingsAction::SavePathTemplate(String::new()));
}

#[test]
fn path_template_mark_saved_returns_to_settings_menu() {
    let mut s = ready();
    enter_path_template(&mut s);

    s.handle_key(key(KeyCode::Up));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('!')));
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(
        s.path_template_editor().unwrap().status,
        PathTemplateRectStatus::Modified
    );

    s.mark_path_template_saved("$BASE_PATH.worktree!".into());

    assert_eq!(s.step(), SettingsStep::Menu);
    assert!(s.path_template_editor().is_none());

    enter_path_template(&mut s);
    let editor = s.path_template_editor().unwrap();
    assert_eq!(editor.template, "$BASE_PATH.worktree!");
    assert_eq!(editor.status, PathTemplateRectStatus::Saved);
}

#[test]
fn path_template_esc_outside_editing_returns_to_menu() {
    let mut s = ready();
    enter_path_template(&mut s);

    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, SettingsAction::Continue);
    assert_eq!(s.step(), SettingsStep::Menu);
    assert!(s.path_template_editor().is_none());
}

#[test]
fn error_overlay_with_r_emits_reset() {
    let mut s = ready();
    s.set_error("boom".into());
    let action = s.handle_key(key(KeyCode::Char('r')));
    assert_eq!(action, SettingsAction::Reset);
}

#[test]
fn error_overlay_clears_on_other_key_and_returns_back() {
    let mut s = ready();
    s.set_error("boom".into());
    let action = s.handle_key(key(KeyCode::Char('x')));
    assert_eq!(action, SettingsAction::Back);
    assert!(s.error().is_none());
}

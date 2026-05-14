//! State-machine + render tests for the Delete Worktree screen.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use wisetree::git::types::{BranchStatus, GitWorktree};
use wisetree::tui::screens::delete::{DeleteAction, DeleteOutcome, DeleteScreen, DeleteStep};

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

fn wt(path: &str, branch: &str, is_main: bool, is_clean: bool) -> GitWorktree {
    GitWorktree {
        path: path.into(),
        branch: branch.into(),
        commit: "deadbeef".into(),
        is_main,
        is_clean,
        branch_status: None,
    }
}

fn worktrees() -> Vec<GitWorktree> {
    vec![
        wt("/tmp/repo", "main", true, true),
        wt("/tmp/repo-feat", "feat", false, true),
        wt("/tmp/repo-bug", "bug", false, true),
    ]
}

#[test]
fn loading_render_shows_loading_message() {
    let s = DeleteScreen::new(false);
    let dumped = dump(60, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("Loading worktrees"));
}

#[test]
fn set_worktrees_filters_main_and_clears_loading() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    assert!(!s.loading());
    assert_eq!(s.worktrees().len(), 2);
    assert!(s.worktrees().iter().all(|w| !w.is_main));
}

#[test]
fn empty_list_cancels_on_keypress() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(vec![wt("/tmp/repo", "main", true, true)]);
    let dumped = dump(60, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("No additional worktrees"));
    let action = s.handle_key(key(KeyCode::Char('x')));
    assert_eq!(action, DeleteAction::Cancelled);
}

#[test]
fn jump_to_confirm_advances_to_confirm_step() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    s.jump_to_confirm_path("/tmp/repo-feat");
    assert_eq!(s.step(), DeleteStep::Confirm);
    assert_eq!(s.selected_path(), Some("/tmp/repo-feat"));
}

#[test]
fn esc_in_confirm_cancels() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    s.jump_to_confirm_path("/tmp/repo-feat");
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, DeleteAction::Cancelled);
}

#[test]
fn confirm_yes_emits_confirmed_with_force_false_for_clean() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    s.jump_to_confirm_path("/tmp/repo-feat");
    s.handle_key(key(KeyCode::Char('y'))); // pre-select Confirm
    let action = s.handle_key(key(KeyCode::Enter));
    match action {
        DeleteAction::Confirmed { path, force } => {
            assert_eq!(path, "/tmp/repo-feat");
            assert!(!force);
        }
        other => panic!("expected Confirmed, got {other:?}"),
    }
}

#[test]
fn confirm_force_when_dirty() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(vec![
        wt("/tmp/repo", "main", true, true),
        wt("/tmp/repo-dirty", "dirty", false, false),
    ]);
    s.jump_to_confirm_path("/tmp/repo-dirty");
    s.handle_key(key(KeyCode::Char('y')));
    let action = s.handle_key(key(KeyCode::Enter));
    match action {
        DeleteAction::Confirmed { path, force } => {
            assert_eq!(path, "/tmp/repo-dirty");
            assert!(force);
        }
        other => panic!("expected Confirmed, got {other:?}"),
    }
}

#[test]
fn deleting_state_renders_branch_in_message() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    s.jump_to_confirm_path("/tmp/repo-feat");
    s.start_deleting();
    let dumped = dump(60, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("Deleting worktree"));
    assert!(dumped.contains("feat"));
}

#[test]
fn success_with_branch_deleted_message() {
    let mut s = DeleteScreen::new(true);
    s.set_worktrees(worktrees());
    s.jump_to_confirm_path("/tmp/repo-feat");
    s.start_deleting();
    s.mark_complete(DeleteOutcome {
        worktree_deleted: true,
        branch_deleted: true,
        branch_name: Some("feat".into()),
    });
    let dumped = dump(80, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("Worktree and branch 'feat' deleted successfully"));
}

#[test]
fn success_with_branch_kept_message() {
    let mut s = DeleteScreen::new(true);
    s.set_worktrees(worktrees());
    s.jump_to_confirm_path("/tmp/repo-feat");
    s.start_deleting();
    s.mark_complete(DeleteOutcome {
        worktree_deleted: true,
        branch_deleted: false,
        branch_name: Some("feat".into()),
    });
    let dumped = dump(80, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("Worktree deleted. Branch 'feat' was kept."));
}

#[test]
fn success_default_message_when_branch_unset() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    s.jump_to_confirm_path("/tmp/repo-feat");
    s.start_deleting();
    s.mark_complete(DeleteOutcome {
        worktree_deleted: true,
        branch_deleted: false,
        branch_name: None,
    });
    let dumped = dump(80, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("Worktree deleted successfully"));
}

#[test]
fn success_enter_returns_done() {
    let mut s = DeleteScreen::new(false);
    s.mark_complete(DeleteOutcome::default());
    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, DeleteAction::Done);
}

#[test]
fn confirm_dialog_shows_branch_status_when_will_delete_branch() {
    let mut s = DeleteScreen::new(true);
    let mut wt_dirty = wt("/tmp/repo-x", "feat-x", false, false);
    wt_dirty.branch_status = Some(BranchStatus {
        ahead: 2,
        behind: 1,
        upstream_branch: Some("origin/feat-x".into()),
    });
    s.set_worktrees(vec![wt("/tmp/repo", "main", true, true), wt_dirty]);
    s.jump_to_confirm_path("/tmp/repo-x");
    let dumped = dump(80, 12, |f| s.render(f, f.area()));
    assert!(dumped.contains("uncommitted changes"));
    assert!(dumped.contains("delete branch 'feat-x'"));
}

#[test]
fn jump_to_bulk_confirm_advances_to_confirm_step() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    s.jump_to_bulk_confirm(vec!["/tmp/repo-feat".into(), "/tmp/repo-bug".into()]);
    assert_eq!(s.step(), DeleteStep::Confirm);
    assert!(s.is_bulk());
}

#[test]
fn bulk_confirm_yes_emits_bulk_confirmed() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    s.jump_to_bulk_confirm(vec!["/tmp/repo-feat".into(), "/tmp/repo-bug".into()]);
    s.handle_key(key(KeyCode::Char('y')));
    let action = s.handle_key(key(KeyCode::Enter));
    match action {
        DeleteAction::BulkConfirmed { items } => {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].0, "/tmp/repo-feat");
            assert_eq!(items[1].0, "/tmp/repo-bug");
        }
        other => panic!("expected BulkConfirmed, got {other:?}"),
    }
}

// -- Bulk delete with checkboxes ----------------------------------------------

fn three_worktrees() -> Vec<GitWorktree> {
    vec![
        wt("/tmp/repo", "main", true, true),
        wt("/tmp/repo-feat", "feat", false, true),
        wt("/tmp/repo-bug", "bug", false, true),
        wt("/tmp/repo-chore", "chore", false, true),
    ]
}

#[test]
fn jump_to_bulk_confirm_starts_with_all_checked() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(three_worktrees());
    s.jump_to_bulk_confirm(vec![
        "/tmp/repo-feat".into(),
        "/tmp/repo-bug".into(),
        "/tmp/repo-chore".into(),
    ]);
    assert_eq!(s.step(), DeleteStep::Confirm);
    let action = s.handle_key(key(KeyCode::Enter));
    match action {
        DeleteAction::BulkConfirmed { items } => {
            assert_eq!(items.len(), 3);
            let paths: Vec<&str> = items.iter().map(|(p, _)| p.as_str()).collect();
            assert_eq!(
                paths,
                vec!["/tmp/repo-feat", "/tmp/repo-bug", "/tmp/repo-chore"]
            );
        }
        other => panic!("expected BulkConfirmed, got {other:?}"),
    }
}

#[test]
fn bulk_confirm_space_toggles_focused_row_and_yes_deletes_subset() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(three_worktrees());
    s.jump_to_bulk_confirm(vec![
        "/tmp/repo-feat".into(),
        "/tmp/repo-bug".into(),
        "/tmp/repo-chore".into(),
    ]);
    // Move down to the second row, uncheck it, then confirm.
    s.handle_key(key(KeyCode::Down));
    s.handle_key(key(KeyCode::Char(' ')));
    let action = s.handle_key(key(KeyCode::Enter));
    match action {
        DeleteAction::BulkConfirmed { items } => {
            let paths: Vec<&str> = items.iter().map(|(p, _)| p.as_str()).collect();
            assert_eq!(paths, vec!["/tmp/repo-feat", "/tmp/repo-chore"]);
        }
        other => panic!("expected BulkConfirmed, got {other:?}"),
    }
}

#[test]
fn bulk_confirm_a_select_all_toggle_round_trips() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(three_worktrees());
    s.jump_to_bulk_confirm(vec!["/tmp/repo-feat".into(), "/tmp/repo-bug".into()]);
    s.handle_key(key(KeyCode::Char(' '))); // uncheck row 0
    s.handle_key(key(KeyCode::Char('a'))); // re-check all
    let action = s.handle_key(key(KeyCode::Enter));
    match action {
        DeleteAction::BulkConfirmed { items } => assert_eq!(items.len(), 2),
        other => panic!("expected BulkConfirmed, got {other:?}"),
    }
}

#[test]
fn bulk_confirm_all_unchecked_returns_cancelled() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(three_worktrees());
    s.jump_to_bulk_confirm(vec!["/tmp/repo-feat".into(), "/tmp/repo-bug".into()]);
    s.handle_key(key(KeyCode::Char('a'))); // uncheck all
    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, DeleteAction::Cancelled);
}

#[test]
fn bulk_confirm_esc_cancels_screen() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(three_worktrees());
    s.jump_to_bulk_confirm(vec!["/tmp/repo-feat".into()]);
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, DeleteAction::Cancelled);
}

#[test]
fn bulk_confirm_render_shows_checkbox_glyphs() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(three_worktrees());
    s.jump_to_bulk_confirm(vec!["/tmp/repo-feat".into(), "/tmp/repo-bug".into()]);
    let dumped = dump(100, 20, |f| s.render(f, f.area()));
    assert!(dumped.contains("☒"));
    assert!(dumped.contains("Are you sure"));
    s.handle_key(key(KeyCode::Char(' '))); // uncheck row 0
    let dumped = dump(100, 20, |f| s.render(f, f.area()));
    assert!(dumped.contains("☐"));
    assert!(dumped.contains("☒"));
}

#[test]
fn bulk_confirm_subset_resets_bulk_total_for_progress() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(three_worktrees());
    s.jump_to_bulk_confirm(vec![
        "/tmp/repo-feat".into(),
        "/tmp/repo-bug".into(),
        "/tmp/repo-chore".into(),
    ]);
    // Uncheck the middle row → 2 of 3 will be confirmed.
    s.handle_key(key(KeyCode::Down));
    s.handle_key(key(KeyCode::Char(' ')));
    let _ = s.handle_key(key(KeyCode::Enter));
    // After confirm, the bulk progress denominator should match the
    // selected subset, not the original 3.
    assert_eq!(s.bulk_progress(), Some((0, 2)));
}

#[test]
fn bulk_deleting_renders_progress() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    s.jump_to_bulk_confirm(vec!["/tmp/repo-feat".into(), "/tmp/repo-bug".into()]);
    s.start_deleting();
    let dumped = dump(80, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("Deleting worktree"));
    assert!(dumped.contains("(1 of 2: feat)"));
}

#[test]
fn bulk_success_message() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    s.jump_to_bulk_confirm(vec!["/tmp/repo-feat".into(), "/tmp/repo-bug".into()]);
    s.bulk_record_progress(None);
    s.bulk_record_progress(None);
    s.mark_bulk_complete();
    let dumped = dump(80, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("2 worktrees deleted successfully"));
}

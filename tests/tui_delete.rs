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
fn select_step_enter_advances_to_confirm() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    assert_eq!(s.step(), DeleteStep::Select);
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), DeleteStep::Confirm);
    assert_eq!(s.selected_path(), Some("/tmp/repo-feat"));
}

#[test]
fn esc_in_select_cancels() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, DeleteAction::Cancelled);
}

#[test]
fn esc_in_confirm_returns_to_select() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Esc));
    assert_eq!(s.step(), DeleteStep::Select);
}

#[test]
fn esc_in_confirm_after_jump_cancels_screen() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    s.jump_to_confirm_path("/tmp/repo-feat");
    assert_eq!(s.step(), DeleteStep::Confirm);
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, DeleteAction::Cancelled);
}

#[test]
fn confirm_yes_emits_confirmed_with_force_false_for_clean() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    s.handle_key(key(KeyCode::Enter));
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
    s.handle_key(key(KeyCode::Enter));
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
    s.handle_key(key(KeyCode::Enter));
    s.start_deleting();
    let dumped = dump(60, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("Deleting worktree"));
    assert!(dumped.contains("feat"));
}

#[test]
fn success_with_branch_deleted_message() {
    let mut s = DeleteScreen::new(true);
    s.set_worktrees(worktrees());
    s.handle_key(key(KeyCode::Enter));
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
    s.handle_key(key(KeyCode::Enter));
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
    s.handle_key(key(KeyCode::Enter));
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
    s.handle_key(key(KeyCode::Enter));
    let dumped = dump(80, 12, |f| s.render(f, f.area()));
    assert!(dumped.contains("uncommitted changes"));
    assert!(dumped.contains("delete branch 'feat-x'"));
}

#[test]
fn error_overlay_clears_and_returns_to_select() {
    let mut s = DeleteScreen::new(false);
    s.set_worktrees(worktrees());
    s.set_error("boom".into());
    assert_eq!(s.step(), DeleteStep::Select);
    let dumped = dump(60, 4, |f| s.render(f, f.area()));
    assert!(dumped.contains("boom"));
    assert!(dumped.contains("Press any key"));
    s.handle_key(key(KeyCode::Char('x')));
    assert!(s.error().is_none());
    assert_eq!(s.step(), DeleteStep::Select);
}

#[test]
fn select_step_preferred_height_grows_to_fit_all_worktrees() {
    let mut s = DeleteScreen::new(false);
    let rows: Vec<GitWorktree> = std::iter::once(wt("/tmp/repo", "main", true, true))
        .chain((0..12).map(|index| {
            wt(
                &format!("/tmp/repo-{index}"),
                &format!("feat-{index}"),
                false,
                true,
            )
        }))
        .collect();
    s.set_worktrees(rows);

    assert_eq!(s.preferred_content_height(), 18);
}

#[test]
fn select_step_uses_available_height_before_scrolling() {
    let mut s = DeleteScreen::new(false);
    let rows: Vec<GitWorktree> = std::iter::once(wt("/tmp/repo", "main", true, true))
        .chain((0..12).map(|index| {
            wt(
                &format!("/tmp/repo-{index}"),
                &format!("feat-{index}"),
                false,
                true,
            )
        }))
        .collect();
    s.set_worktrees(rows);

    let dumped = dump(100, 18, |f| s.render(f, f.area()));
    assert!(dumped.contains("repo-11"));
    assert!(!dumped.contains("more below"));
    assert!(!dumped.contains("more above"));
}

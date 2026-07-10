//! Update Pull Request confirmation screen. Three-step state machine:
//!
//! - `Loading`  : spinner while `App` resolves the base ref against the
//!   priority list (`upstream/main → upstream/master → origin/main →
//!   origin/master`).
//! - `Confirm`  : details panel on top, `ConfirmationModal` (Yes/No,
//!   **No** default) on the bottom. Enter on Yes returns
//!   `UpdateAction::Confirmed`.
//! - `Updating` : spinner with a phase-specific label on top, plus a
//!   bordered "AI Activity" panel that streams the opencode subprocess's
//!   stdout/stderr lines as they arrive (auto-scrolled to the latest). The
//!   conflict-resolution TUI never exits on its own, so the App watches
//!   opencode's database with an `OpencodeTurnWatcher` and marks the AI
//!   done automatically when the turn completes (a PTY exit or the manual
//!   "Merge finalized?" confirm both do the same as a fallback). Once the
//!   AI is done the panel grows a `[ Complete ] [ Cancel ]` button row at
//!   the bottom; **Complete** commits + pushes the AI resolution,
//!   **Cancel** aborts the merge.
//!
//! Async work is owned by `App`; this screen is purely a presentation
//! state machine.

use std::cell::Cell;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::messages::colors;
use crate::services::dashboard::{AiActivityEvent, AiActivitySeverity, AiToolResultStatus};
use crate::tui::screens::dashboard::UpdatePullRequestRequest;
use crate::tui::widgets::{
    render_summary_table, ConfirmationChoice, ConfirmationModal, ConfirmationOutcome, PtyView,
    Status, StatusIndicator, SummaryRow,
};

const UPDATE_LOADING_MESSAGE: &str = "Resolving base ref...";
const UPDATE_RUNNING_MESSAGE: &str = "Updating pull request...";
const UPDATE_PUSH_FAILED_MESSAGE: &str =
    "Push failed — fix it in the terminal, then Accept to retry";

/// Hard cap on the number of AI activity lines retained in memory. A
/// long opencode run can emit thousands of rows (tool calls, file edits,
/// progress dots); we only ever render the bottom slice that fits the
/// activity panel, so anything older is pure memory pressure.
const AI_LOG_MAX_LINES: usize = 1024;

/// CSI sequences for PageUp / PageDown forwarded to the embedded opencode
/// process. Mouse wheel + arrow keys both synthesize these so the user
/// scrolls opencode's own message buffer (vt100's alt-screen scrollback
/// is unusable while opencode owns the alternate grid).
const PTY_PAGE_UP: &[u8] = b"\x1b[5~";
const PTY_PAGE_DOWN: &[u8] = b"\x1b[6~";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStep {
    Loading,
    Confirm,
    Updating,
    /// Commit + push of the AI-resolved merge runs in a live PTY so the user
    /// sees git hooks, push progress, and any errors in real time. Transitions
    /// to a done summary once the child exits.
    CommitPush,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiButton {
    Complete,
    Cancel,
}

/// Focused button in the Terminal Activity recovery panel's decision row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermButton {
    /// Re-attempt `git push origin HEAD` and report the real outcome.
    Accept,
    /// Leave the terminal without re-pushing; back to the dashboard.
    Discard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateAction {
    Continue,
    Cancelled,
    Confirmed,
    /// User pressed `Complete` after the AI finished — commit + push.
    AiComplete,
    /// User pressed `Cancel` after the AI finished — abort merge.
    AiCancel,
    /// User accepted in the Terminal Activity recovery panel — re-run
    /// `git push origin HEAD` and report success/failure.
    TerminalAccept,
    /// User left the Terminal Activity recovery panel without re-pushing.
    TerminalDiscard,
}

pub struct UpdatePullRequestScreen {
    request: UpdatePullRequestRequest,
    confirm: Option<ConfirmationModal>,
    /// Scroll offset from the bottom of the AI activity log. `0` means
    /// "follow the latest output". When the user wheels upward we increase
    /// this offset and preserve it as new lines arrive so the viewport stays
    /// stable instead of snapping back to the tail.
    ai_scroll: u16,
    /// Label shown next to the spinner during `Updating`. Updated as the
    /// pipeline emits `UpdatePhase` events so the user knows whether
    /// we're fetching, merging, waiting on the AI, or committing.
    phase_message: String,
    /// Streaming log of the AI subprocess output. Capped at
    /// `AI_LOG_MAX_LINES`; the activity panel always renders the bottom
    /// slice so the latest line stays visible.
    ai_log: Vec<AiActivityEvent>,
    /// `true` once the pipeline has reached `ConflictsDetected` and the
    /// AI is about to (or already started) work on the merge. Drives the
    /// AI Activity panel: when `false`, the `Updating` step renders just
    /// the spinner so the panel doesn't appear during clean merges.
    ai_active: bool,
    /// `true` once opencode has exited and the user is being asked to
    /// decide on Complete or Cancel.
    ai_done: bool,
    /// Currently focused button in the Complete/Cancel pair.
    ai_button: AiButton,
    /// Embedded opencode subprocess + vt100 emulator. `Some` once the
    /// pipeline reached `ConflictsHandedOffToUi` and the App handed us
    /// the spawn parameters; the PTY is rendered into the AI Activity
    /// panel and torn down via `Drop` when the screen is replaced.
    pty: Option<PtyView>,
    /// Which terminal owns keyboard input while the embedded opencode TUI
    /// is alive. `false` = outer Wisetree (default), `true` = inner PTY
    /// so the user can chat with opencode directly. Tab toggles.
    pty_focused: bool,
    /// When `Some`, an overlay confirmation modal is open asking the user
    /// to confirm that the AI resolution is finalized. Reachable from the
    /// Outer (Wisetree) focus during the streaming phase by pressing
    /// Enter; the modal swallows keys until the user picks Yes (transition
    /// to the Complete/Cancel review buttons) or No/Esc (resume the AI
    /// Activity panel). The embedded opencode PTY keeps running
    /// underneath either way because `App::tick_pty` ticks every frame.
    finalize_confirm: Option<ConfirmationModal>,
    /// `true` when this screen drives a *push-only* flow (the dashboard's
    /// "Push Pull Request" action) instead of the full fetch/merge/push
    /// update. Changes the confirm prompt + steps preview and routes the
    /// App to `kick_off_push_pull_request` on confirmation.
    push_only: bool,
    /// `true` when this screen drives the dashboard's "Update branch
    /// (locally)" conflict tail rather than a pull-request update. The
    /// fetch/merge already happened behind the dashboard splash; this
    /// screen only hosts the opencode resolution + a local `git commit`
    /// (no push). Drives "push"-free wording on the commit + done pages.
    local_only: bool,
    /// `true` once a `git push` failed and the screen handed off to the
    /// interactive recovery shell. Drives the "Terminal Activity" panel:
    /// the embedded `pty` hosts the user's shell, and the Accept/Discard
    /// button row is always available (no child-exit gating, unlike AI).
    terminal_active: bool,
    /// Currently focused button in the Accept/Discard pair while the
    /// Terminal Activity panel is open.
    terminal_button: TermButton,
    /// The `git push` failure that opened the Terminal Activity panel,
    /// shown as a one-line header above the shell so the user sees why
    /// they were dropped into the terminal.
    terminal_error: String,
    error: Option<String>,
    step: UpdateStep,
    ai_button_rects: Cell<[Rect; 2]>,
    terminal_button_rects: Cell<[Rect; 2]>,
    pub tick: usize,
    /// `true` once the commit+push PTY child has exited.
    commit_push_done: bool,
    /// `true` when the commit+push shell exited with code 0.
    commit_push_succeeded: bool,
    /// Summary rows shown on the done page (one row: overall commit+push result).
    commit_push_summary: Vec<SummaryRow>,
}

impl UpdatePullRequestScreen {
    pub fn new(request: UpdatePullRequestRequest) -> Self {
        // If the caller already resolved the base ref (rare — usually the
        // app kicks off resolution after mounting), jump straight to
        // Confirm. Otherwise show a loading spinner until `set_base_ref`
        // fires.
        let (confirm, step) = if request.base_ref.is_some() {
            (Some(build_confirm(&request, false)), UpdateStep::Confirm)
        } else {
            (None, UpdateStep::Loading)
        };
        Self {
            request,
            confirm,
            ai_scroll: 0,
            phase_message: UPDATE_RUNNING_MESSAGE.to_string(),
            ai_log: Vec::new(),
            ai_active: false,
            ai_done: false,
            ai_button: AiButton::Complete,
            pty: None,
            pty_focused: false,
            finalize_confirm: None,
            push_only: false,
            local_only: false,
            terminal_active: false,
            terminal_button: TermButton::Accept,
            terminal_error: String::new(),
            error: None,
            step,
            ai_button_rects: Cell::new([Rect::default(); 2]),
            terminal_button_rects: Cell::new([Rect::default(); 2]),
            tick: 0,
            commit_push_done: false,
            commit_push_succeeded: false,
            commit_push_summary: Vec::new(),
        }
    }

    /// Construct the screen for the push-only flow (dashboard "Push Pull
    /// Request" action). A push needs no base ref, so we land straight on
    /// the Confirm step regardless of whether `base_ref` was populated.
    pub fn new_push(request: UpdatePullRequestRequest) -> Self {
        let confirm = build_confirm(&request, true);
        Self {
            push_only: true,
            confirm: Some(confirm),
            step: UpdateStep::Confirm,
            ..Self::new(request)
        }
    }

    /// Construct the screen for the "Update branch (locally)" conflict
    /// tail. The dashboard already ran fetch + merge and hit conflicts, so
    /// we skip Loading/Confirm and land directly on the `Updating` step
    /// with the AI panel active. The caller spawns the opencode PTY via
    /// `spawn_opencode_pty` right after constructing. Finishing commits the
    /// merge locally (no push).
    pub fn new_local_conflict(request: UpdatePullRequestRequest) -> Self {
        Self {
            local_only: true,
            step: UpdateStep::Updating,
            ai_active: true,
            ..Self::new(request)
        }
    }

    pub fn request(&self) -> &UpdatePullRequestRequest {
        &self.request
    }

    pub fn is_push_only(&self) -> bool {
        self.push_only
    }

    /// `true` when this screen is resolving conflicts for a local branch
    /// update (commit, no push) rather than a pull-request update.
    pub fn local_only(&self) -> bool {
        self.local_only
    }

    pub fn terminal_active(&self) -> bool {
        self.terminal_active
    }

    pub fn step(&self) -> UpdateStep {
        self.step
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Called by `App` once the background resolver has picked a
    /// reachable base ref. Builds the confirm dialog and transitions to
    /// the `Confirm` step.
    pub fn set_base_ref(&mut self, base_ref: String) {
        self.request.base_ref = Some(base_ref);
        self.confirm = Some(build_confirm(&self.request, self.push_only));
        self.error = None;
        self.step = UpdateStep::Confirm;
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.step = UpdateStep::Confirm;
        self.confirm = None;
    }

    pub fn start_updating(&mut self) {
        self.step = UpdateStep::Updating;
        self.phase_message = UPDATE_RUNNING_MESSAGE.to_string();
        self.ai_log.clear();
        self.ai_scroll = 0;
        self.ai_active = false;
        self.ai_done = false;
        self.ai_button = AiButton::Complete;
        self.pty = None;
        self.pty_focused = false;
        self.finalize_confirm = None;
        self.terminal_active = false;
        self.terminal_button = TermButton::Accept;
        self.terminal_error.clear();
    }

    /// Hand off to the interactive recovery shell after a `git push`
    /// failure. Spawns the user's shell inside the embedded PTY (rooted at
    /// the worktree), seeds it with the failing `git push origin HEAD` so
    /// the panel reproduces the error live, and flips into the Terminal
    /// Activity layout. From here the user can run any command, then
    /// Accept (re-push) or Discard (leave). A spawn failure leaves the
    /// panel up with the error header so the user still sees what went
    /// wrong (and can Discard back to the dashboard).
    pub fn start_terminal_recovery(
        &mut self,
        shell: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        error: String,
    ) {
        self.step = UpdateStep::Updating;
        self.ai_active = false;
        self.ai_done = false;
        self.terminal_active = true;
        self.terminal_button = TermButton::Accept;
        self.terminal_error = error;
        self.pty_focused = false;
        self.phase_message = UPDATE_PUSH_FAILED_MESSAGE.to_string();
        match PtyView::spawn(&shell, &args, Some(&cwd), &[]) {
            Ok(mut pty) => {
                // Reproduce the failing push in the panel so the user lands
                // on the real error, then leaves them at a live prompt.
                pty.send_input(b"git push origin HEAD\r");
                self.pty = Some(pty);
            }
            Err(err) => {
                self.pty = None;
                if self.terminal_error.is_empty() {
                    self.terminal_error = format!("Could not spawn shell: {err}");
                }
            }
        }
    }

    /// Spawn the opencode subprocess inside an embedded PTY and route
    /// its raw output through vt100 so the user sees the real opencode
    /// TUI (formatted assistant text, thinking blocks, tool calls)
    /// inside the AI Activity panel. The App invokes this once the
    /// service pipeline returns `ConflictsHandedOffToUi`. Failure to
    /// spawn surfaces as a Notice in the AI log; the user will still see
    /// the Complete/Cancel buttons once the (empty) PTY is reaped.
    pub fn spawn_opencode_pty(
        &mut self,
        binary: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    ) {
        match PtyView::spawn(&binary, &args, Some(&cwd), &env) {
            Ok(pty) => {
                self.pty = Some(pty);
            }
            Err(err) => {
                self.append_ai_line(AiActivityEvent::Notice {
                    severity: AiActivitySeverity::Error,
                    message: format!("Could not spawn opencode in PTY: {err}"),
                });
                // No PTY → the AI is effectively done; surface
                // Complete/Cancel so the user can recover.
                self.mark_ai_done();
            }
        }
    }

    /// Called every frame by the App. Polls the embedded opencode PTY
    /// for child exit and resizes the PTY to match the AI Activity
    /// panel's inner area when it changes. Returns `true` exactly once
    /// — on the tick where the child transitions to exited — so the App
    /// can flip the screen into the Complete/Cancel decision step.
    pub fn tick_pty(&mut self, panel_inner: Option<(u16, u16)>) -> bool {
        let Some(pty) = self.pty.as_mut() else {
            return false;
        };
        if let Some((rows, cols)) = panel_inner {
            pty.resize(rows, cols);
        }
        if pty.poll_exited() {
            // Commit+push shell finished — record the exit code and flip
            // into the done summary view.
            if matches!(self.step, UpdateStep::CommitPush) {
                let code = pty.exit_code();
                self.mark_commit_push_done(code);
                return true;
            }
            // In the Terminal Activity recovery flow the user may type
            // `exit`, killing the shell. That is *not* the AI-done signal —
            // the Accept/Discard buttons are already on screen, so we just
            // release inner focus and leave the rest of the panel intact.
            if self.terminal_active {
                self.pty_focused = false;
                return true;
            }
            self.mark_ai_done();
            return true;
        }
        false
    }

    /// Flip on the AI Activity panel. Called by the App once the pipeline
    /// has surfaced the "handing off to AI" toast (i.e. `ConflictsDetected`),
    /// so the panel only appears for runs that actually need the AI.
    pub fn mark_ai_active(&mut self) {
        self.ai_active = true;
    }

    /// Called by the App once opencode has exited. Surfaces the
    /// Complete / Cancel button row so the user can commit or abort.
    ///
    /// Idempotent on purpose: `tick_pty` polls the PTY every frame and
    /// `PtyView::poll_exited` returns true on every poll after the child
    /// has exited (the underlying `done` flag stays set). Without the
    /// early return, the user's `→` press to focus Cancel would be
    /// undone the very next tick when this method reset `ai_button`
    /// back to `Complete`.
    pub fn mark_ai_done(&mut self) {
        if self.ai_done {
            return;
        }
        self.ai_done = true;
        self.ai_button = AiButton::Complete;
    }

    pub fn ai_active(&self) -> bool {
        self.ai_active
    }

    pub fn is_pty_focused(&self) -> bool {
        self.pty_focused
    }

    /// Whether the embedded opencode subprocess/PTY is currently alive. The
    /// App watches this to force a full terminal repaint once the PTY tears
    /// down, preventing stale scrollback from bleeding into static regions
    /// under the inline `Viewport::Fixed` renderer.
    pub fn has_pty(&self) -> bool {
        self.pty.is_some()
    }

    /// `true` once opencode has exited (the AI Activity panel is showing the
    /// Complete/Cancel decision). Read by the "Update all" batch driver to
    /// know when to auto-commit and advance.
    pub fn ai_done(&self) -> bool {
        self.ai_done
    }

    #[cfg(test)]
    pub(crate) fn ai_button(&self) -> AiButton {
        self.ai_button
    }

    #[cfg(test)]
    pub(crate) fn terminal_button(&self) -> TermButton {
        self.terminal_button
    }

    /// Flip the screen into Terminal Activity mode without spawning a real
    /// shell, so the outer-focus key/button logic can be exercised
    /// deterministically in tests.
    #[cfg(test)]
    pub(crate) fn enter_terminal_mode_for_test(&mut self) {
        self.step = UpdateStep::Updating;
        self.ai_active = false;
        self.ai_done = false;
        self.terminal_active = true;
        self.terminal_button = TermButton::Accept;
        self.terminal_error = "remote rejected the push".to_string();
        self.pty = None;
        self.pty_focused = false;
    }

    pub fn is_updating(&self) -> bool {
        matches!(self.step, UpdateStep::Updating)
    }

    /// `true` while the commit+push PTY is running (before the child exits).
    /// Used by `App` to decide whether to give the panel full-screen height.
    pub fn commit_push_running(&self) -> bool {
        matches!(self.step, UpdateStep::CommitPush) && !self.commit_push_done
    }

    /// `true` once the commit(+push) PTY has exited. Read by the "Update all"
    /// batch driver to know the conflict resolution for the current worktree
    /// is finished so it can advance to the next one.
    pub fn commit_push_done(&self) -> bool {
        self.commit_push_done
    }

    /// Whether the finished commit(+push) exited 0. Only meaningful once
    /// [`Self::commit_push_done`] is `true`.
    pub fn commit_push_succeeded(&self) -> bool {
        self.commit_push_succeeded
    }

    /// Spawn a shell that runs `git add -A && git commit && git push` inside a
    /// live PTY so the user sees all output (hooks, progress, errors) in real
    /// time. `shell` + `shell_args` are the login-shell wrapper produced by
    /// `login_shell_command`; `cwd` is the worktree; `env` carries at minimum
    /// `COMMIT_MSG` so the script never needs to escape the message.
    pub fn start_commit_push_pty(
        &mut self,
        shell: PathBuf,
        shell_args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    ) {
        self.step = UpdateStep::CommitPush;
        self.commit_push_done = false;
        self.commit_push_succeeded = false;
        self.commit_push_summary = Vec::new();
        // The AI phase is over; clear the flag so the Cancelled handler in
        // `App` does not try to abort a merge that is already committed.
        self.ai_active = false;
        self.pty_focused = false;
        match PtyView::spawn(&shell, &shell_args, Some(&cwd), &env) {
            Ok(pty) => {
                self.pty = Some(pty);
            }
            Err(err) => {
                self.pty = None;
                self.commit_push_summary = vec![SummaryRow::failure(
                    self.commit_action_label(),
                    format!("Could not spawn shell: {err}"),
                )];
                self.commit_push_done = true;
            }
        }
    }

    /// Summary-row label for the finalize step. Drops "Push" for local
    /// branch updates, which commit but never push.
    fn commit_action_label(&self) -> &'static str {
        if self.local_only {
            "Commit AI resolution"
        } else {
            "Commit & Push AI resolution"
        }
    }

    /// Called by `tick_pty` once the commit (+ push) child exits. Builds the
    /// one-row summary table and flips into the done view.
    fn mark_commit_push_done(&mut self, exit_code: Option<i32>) {
        let succeeded = exit_code == Some(0);
        self.commit_push_succeeded = succeeded;
        self.commit_push_summary = if succeeded {
            vec![SummaryRow::success(self.commit_action_label())]
        } else {
            vec![SummaryRow::failure(
                self.commit_action_label(),
                "See terminal output above",
            )]
        };
        self.commit_push_done = true;
        self.pty_focused = false;
    }

    /// Update the spinner label during `Updating` so the user knows what
    /// the pipeline is actively doing (fetching, merging, AI resolving,
    /// committing, …). No-op outside the `Updating` step.
    pub fn set_phase_message(&mut self, message: impl Into<String>) {
        let message = message.into();
        if message.is_empty() {
            return;
        }
        self.phase_message = message;
    }

    /// Append a streamed AI activity event to the log. Consecutive assistant /
    /// thinking deltas are coalesced so the panel reads as a sentence instead
    /// of a pile of token-sized fragments.
    pub fn append_ai_line(&mut self, line: impl Into<AiActivityEvent>) {
        let line = line.into();
        if line.plain_text().is_empty() {
            return;
        }
        match (self.ai_log.last_mut(), &line) {
            (
                Some(AiActivityEvent::AssistantText { content: existing }),
                AiActivityEvent::AssistantText { content },
            ) => {
                existing.push_str(content);
                return;
            }
            (
                Some(AiActivityEvent::Thinking { content: existing }),
                AiActivityEvent::Thinking { content },
            ) => {
                existing.push_str(content);
                return;
            }
            _ => {}
        }
        if self.ai_scroll > 0 {
            self.ai_scroll = self.ai_scroll.saturating_add(1);
        }
        self.ai_log.push(line);
        // Trim from the front so the latest output is always retained.
        if self.ai_log.len() > AI_LOG_MAX_LINES {
            let drop = self.ai_log.len() - AI_LOG_MAX_LINES;
            self.ai_log.drain(0..drop);
            self.ai_scroll = self.ai_scroll.saturating_sub(drop as u16);
        }
    }

    #[cfg(test)]
    pub(crate) fn ai_log_lines(&self) -> Vec<String> {
        self.ai_log
            .iter()
            .map(AiActivityEvent::plain_text)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn phase_message(&self) -> &str {
        &self.phase_message
    }

    /// Scroll the AI Activity panel up by `lines` (only meaningful while
    /// the AI is actively streaming or has just finished). When the
    /// embedded PTY is alive, opencode runs on the alt-screen so vt100's
    /// own scrollback is empty — we forward a PageUp keystroke to the
    /// subprocess instead, which fires opencode's own message-history
    /// scroll binding. Without a PTY we fall back to the structured-event
    /// log offset.
    pub fn handle_mouse_scroll_up(&mut self, lines: u16) -> bool {
        let scrollable = (matches!(self.step, UpdateStep::Updating)
            && (self.ai_active || self.terminal_active))
            || (matches!(self.step, UpdateStep::CommitPush) && !self.commit_push_done);
        if !scrollable {
            return false;
        }
        // The commit+push shell and the terminal-recovery shell both run on
        // the main vt100 screen (not alt-screen), so scroll the vt100 buffer
        // directly. The opencode PTY uses alt-screen, so we forward PageUp as
        // a keystroke and opencode handles its own scroll.
        let use_direct_scroll = self.terminal_active || matches!(self.step, UpdateStep::CommitPush);
        if let Some(pty) = self.pty.as_mut() {
            if use_direct_scroll {
                pty.scroll_up(lines);
            } else {
                pty.send_input(PTY_PAGE_UP);
            }
        } else if matches!(self.step, UpdateStep::Updating) {
            self.ai_scroll = self.ai_scroll.saturating_add(lines);
        }
        true
    }

    /// Scroll the panel down by `lines`. The render path clamps against the
    /// content height every frame, so over-scrolling is safe here.
    pub fn handle_mouse_scroll_down(&mut self, lines: u16) -> bool {
        let scrollable = (matches!(self.step, UpdateStep::Updating)
            && (self.ai_active || self.terminal_active))
            || (matches!(self.step, UpdateStep::CommitPush) && !self.commit_push_done);
        if !scrollable {
            return false;
        }
        let use_direct_scroll = self.terminal_active || matches!(self.step, UpdateStep::CommitPush);
        if let Some(pty) = self.pty.as_mut() {
            if use_direct_scroll {
                pty.scroll_down(lines);
            } else {
                pty.send_input(PTY_PAGE_DOWN);
            }
        } else if matches!(self.step, UpdateStep::Updating) {
            self.ai_scroll = self.ai_scroll.saturating_sub(lines);
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn ai_scroll(&self) -> u16 {
        self.ai_scroll
    }

    /// Page size used for PgUp/PgDn scrolling. The PTY's panel area is
    /// only known at render time, so we use a sensible constant that
    /// matches the typical visible row count of the AI Activity panel.
    /// Roughly half a screen — enough to feel snappy without losing the
    /// reader's place.
    const KEYBOARD_PAGE_SCROLL: u16 = 10;
    const KEYBOARD_LINE_SCROLL: u16 = 1;

    /// Handle scroll-only keys when the outer (Wisetree) terminal owns
    /// focus. Returns true when the key was consumed as a scroll action.
    fn handle_outer_scroll_key(&mut self, key: &KeyEvent) -> bool {
        let scrollable = self.ai_active
            || self.terminal_active
            || (matches!(self.step, UpdateStep::CommitPush) && !self.commit_push_done);
        if !scrollable {
            return false;
        }
        match key.code {
            KeyCode::PageUp => {
                self.handle_mouse_scroll_up(Self::KEYBOARD_PAGE_SCROLL);
                true
            }
            KeyCode::PageDown => {
                self.handle_mouse_scroll_down(Self::KEYBOARD_PAGE_SCROLL);
                true
            }
            KeyCode::Up => {
                self.handle_mouse_scroll_up(Self::KEYBOARD_LINE_SCROLL);
                true
            }
            KeyCode::Down => {
                self.handle_mouse_scroll_down(Self::KEYBOARD_LINE_SCROLL);
                true
            }
            KeyCode::Home => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.scroll_to_top();
                }
                true
            }
            KeyCode::End => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.scroll_to_bottom();
                }
                true
            }
            _ => false,
        }
    }

    /// Route a keystroke into the finalize-confirmation modal. Caller
    /// must have already established that the modal is open. Tab / ← /
    /// → toggle the selection; Enter commits (Yes flips the screen into
    /// the Complete/Cancel review state, No simply closes the modal);
    /// Esc closes the modal without acting (same as picking No).
    fn handle_finalize_modal_key(&mut self, key: KeyEvent) -> UpdateAction {
        let modal = self
            .finalize_confirm
            .as_mut()
            .expect("handle_finalize_modal_key called with no modal open");
        match modal.handle_key(key) {
            ConfirmationOutcome::Pending => {}
            ConfirmationOutcome::Confirmed => {
                self.finalize_confirm = None;
                self.mark_ai_done();
            }
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                self.finalize_confirm = None;
            }
        }
        UpdateAction::Continue
    }

    #[cfg(test)]
    pub(crate) fn finalize_confirm(&self) -> Option<ConfirmationChoice> {
        self.finalize_confirm.as_ref().map(|m| m.selected())
    }

    /// Keyboard handling while the Terminal Activity recovery panel owns the
    /// screen. Mirrors the AI panel's split-focus model: Tab toggles between
    /// the outer Wisetree TUI and the inner shell; inner focus forwards every
    /// keystroke to the shell. With outer focus, ←/→ switch Accept/Discard,
    /// the scroll keys page the shell's scrollback, Enter acts on the focused
    /// button, and Esc discards.
    fn handle_terminal_key(&mut self, key: KeyEvent) -> UpdateAction {
        if self.pty.is_some() && matches!(key.code, KeyCode::Tab) {
            self.pty_focused = !self.pty_focused;
            return UpdateAction::Continue;
        }
        if self.pty_focused {
            if let Some(pty) = self.pty.as_mut() {
                if let Some(bytes) = key_event_to_pty_bytes(&key) {
                    pty.send_input(&bytes);
                }
            }
            return UpdateAction::Continue;
        }
        if self.handle_outer_scroll_key(&key) {
            return UpdateAction::Continue;
        }
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::BackTab => {
                self.terminal_button = match self.terminal_button {
                    TermButton::Accept => TermButton::Discard,
                    TermButton::Discard => TermButton::Accept,
                };
                UpdateAction::Continue
            }
            KeyCode::Enter => match self.terminal_button {
                TermButton::Accept => UpdateAction::TerminalAccept,
                TermButton::Discard => UpdateAction::TerminalDiscard,
            },
            KeyCode::Esc => UpdateAction::TerminalDiscard,
            _ => UpdateAction::Continue,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> UpdateAction {
        if self.error.is_some() {
            return UpdateAction::Cancelled;
        }
        if matches!(self.step, UpdateStep::Loading) {
            return match key.code {
                KeyCode::Esc => UpdateAction::Cancelled,
                _ => UpdateAction::Continue,
            };
        }
        if matches!(self.step, UpdateStep::CommitPush) {
            // Done: any key goes back to the dashboard.
            if self.commit_push_done {
                return UpdateAction::Cancelled;
            }
            // Running: allow scrolling the PTY, swallow everything else so
            // the user can't accidentally navigate away mid-commit.
            if self.handle_outer_scroll_key(&key) {
                return UpdateAction::Continue;
            }
            return UpdateAction::Continue;
        }
        if matches!(self.step, UpdateStep::Updating) {
            // The Terminal Activity recovery panel has its own focus + button
            // model (Accept/Discard always available, no child-exit gating),
            // so it short-circuits the AI handling below.
            if self.terminal_active {
                return self.handle_terminal_key(key);
            }
            if !self.ai_done {
                // The finalize-confirmation modal — when open — owns all
                // input. Tab/←/→ toggle Yes↔No, Enter commits the choice,
                // Esc dismisses. The PTY keeps running underneath via
                // `App::tick_pty`, so opencode is unaffected by the modal.
                if self.finalize_confirm.is_some() {
                    return self.handle_finalize_modal_key(key);
                }
                // While opencode is running we let the user split focus
                // between the outer Wisetree TUI and the inner PTY: Tab
                // toggles, and when the PTY owns focus we transparently
                // forward keystrokes to the subprocess so the user can
                // chat with opencode just like they would standalone.
                if self.pty.is_some() && matches!(key.code, KeyCode::Tab) {
                    self.pty_focused = !self.pty_focused;
                    return UpdateAction::Continue;
                }
                if self.pty_focused {
                    if let Some(pty) = self.pty.as_mut() {
                        // Forward keystrokes verbatim so opencode's own
                        // context-aware bindings (menu navigation in
                        // pickers like Ctrl+P, prompt-history in the
                        // chat input, PgUp/PgDn for scroll) all work as
                        // they would in a standalone terminal. Outer
                        // focus has its own line-by-line scroll path.
                        if let Some(bytes) = key_event_to_pty_bytes(&key) {
                            pty.send_input(&bytes);
                        }
                    }
                    return UpdateAction::Continue;
                }
                if self.handle_outer_scroll_key(&key) {
                    return UpdateAction::Continue;
                }
                // Outer focus + AI streaming + Enter is the manual
                // "I'm done with opencode, let me review and decide" cue —
                // a fallback for when the App's `OpencodeTurnWatcher` hasn't
                // detected completion yet (it normally marks the AI done
                // automatically). We don't transition straight to the
                // review buttons — an accidental Enter while opencode is
                // mid-edit would be destructive — so we surface a
                // confirmation modal first. The PTY keeps running underneath.
                if self.ai_active && matches!(key.code, KeyCode::Enter) {
                    self.finalize_confirm = Some(build_finalize_modal());
                    return UpdateAction::Continue;
                }
                // Outer-focus & still streaming → swallow other keys so
                // the user doesn't accidentally trigger Wisetree actions
                // mid-resolution.
                return UpdateAction::Continue;
            }
            if self.handle_outer_scroll_key(&key) {
                return UpdateAction::Continue;
            }
            return match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    self.ai_button = match self.ai_button {
                        AiButton::Complete => AiButton::Cancel,
                        AiButton::Cancel => AiButton::Complete,
                    };
                    UpdateAction::Continue
                }
                KeyCode::Enter => match self.ai_button {
                    AiButton::Complete => UpdateAction::AiComplete,
                    AiButton::Cancel => UpdateAction::AiCancel,
                },
                KeyCode::Char('c') | KeyCode::Char('C') => UpdateAction::AiComplete,
                KeyCode::Char('x') | KeyCode::Char('X') | KeyCode::Esc => UpdateAction::AiCancel,
                _ => UpdateAction::Continue,
            };
        }
        let dialog = match self.confirm.as_mut() {
            Some(d) => d,
            None => return UpdateAction::Cancelled,
        };
        match dialog.handle_key(key) {
            ConfirmationOutcome::Confirmed => UpdateAction::Confirmed,
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                UpdateAction::Cancelled
            }
            ConfirmationOutcome::Pending => UpdateAction::Continue,
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> UpdateAction {
        if self.error.is_some() || matches!(self.step, UpdateStep::Loading) {
            return UpdateAction::Continue;
        }
        if matches!(self.step, UpdateStep::CommitPush) {
            if self.commit_push_done {
                return UpdateAction::Cancelled;
            }
            return UpdateAction::Continue;
        }
        if matches!(self.step, UpdateStep::Updating) {
            if self.terminal_active {
                let [accept_rect, discard_rect] = self.terminal_button_rects.get();
                if contains_position(accept_rect, position) {
                    self.terminal_button = TermButton::Accept;
                    return UpdateAction::TerminalAccept;
                }
                if contains_position(discard_rect, position) {
                    self.terminal_button = TermButton::Discard;
                    return UpdateAction::TerminalDiscard;
                }
                return UpdateAction::Continue;
            }
            if let Some(modal) = self.finalize_confirm.as_mut() {
                return match modal.handle_mouse_click(position) {
                    ConfirmationOutcome::Pending => UpdateAction::Continue,
                    ConfirmationOutcome::Confirmed => {
                        self.finalize_confirm = None;
                        self.mark_ai_done();
                        UpdateAction::Continue
                    }
                    ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                        self.finalize_confirm = None;
                        UpdateAction::Continue
                    }
                };
            }
            if !self.ai_done {
                return UpdateAction::Continue;
            }
            let [complete_rect, cancel_rect] = self.ai_button_rects.get();
            if contains_position(complete_rect, position) {
                self.ai_button = AiButton::Complete;
                return UpdateAction::AiComplete;
            }
            if contains_position(cancel_rect, position) {
                self.ai_button = AiButton::Cancel;
                return UpdateAction::AiCancel;
            }
            return UpdateAction::Continue;
        }
        let Some(dialog) = self.confirm.as_mut() else {
            return UpdateAction::Cancelled;
        };
        match dialog.handle_mouse_click(position) {
            ConfirmationOutcome::Confirmed => UpdateAction::Confirmed,
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                UpdateAction::Cancelled
            }
            ConfirmationOutcome::Pending => UpdateAction::Continue,
        }
    }

    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            UpdateStep::Loading => 3,
            UpdateStep::CommitPush => {
                if self.commit_push_done {
                    // StatusIndicator (3) + summary table (4) + hint (1)
                    8
                } else {
                    28 // full PTY panel (same as terminal_active)
                }
            }
            UpdateStep::Updating => {
                // Pre-conflict phases (fetching, merging, pushing-clean)
                // don't need the AI Activity panel — keep the panel tall
                // only once we've flipped into AI mode so the streaming
                // output has room to breathe. The Terminal Activity recovery
                // panel needs the same generous height (shell + buttons).
                if self.terminal_active {
                    28
                } else if self.ai_active {
                    if self.ai_done {
                        29
                    } else {
                        25
                    }
                } else {
                    3
                }
            }
            UpdateStep::Confirm => {
                let detail_rows = self.detail_line_count() as u16;
                let steps_rows = self.steps_line_count() as u16;
                detail_rows
                    .saturating_add(steps_rows)
                    .saturating_add(14)
                    .max(16)
            }
        }
    }

    fn detail_line_count(&self) -> usize {
        // PR / Title / URL / Branch / Worktree are always shown.
        let mut rows = 5;
        if self.request.base_ref.is_some() {
            rows += 1;
        }
        if self.request.behind > 0 {
            rows += 1;
        }
        if self.request.ahead > 0 {
            rows += 1;
        }
        rows
    }

    fn steps_line_count(&self) -> usize {
        if self.push_only {
            // header + 1 bullet
            2
        } else {
            // header + 4 bullets
            5
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(err) = self.error.as_deref() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(2), Constraint::Length(1)])
                .split(area);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("Cannot update pull request: {err}"),
                    Style::default().fg(colors::ERROR),
                ))),
                chunks[0],
            );
            frame.render_widget(
                Paragraph::new("Press any key to return to dashboard...").style(
                    Style::default()
                        .fg(colors::MUTED)
                        .add_modifier(Modifier::DIM),
                ),
                chunks[1],
            );
            return;
        }
        match self.step {
            UpdateStep::Loading => {
                StatusIndicator::new(Status::Loading, UPDATE_LOADING_MESSAGE)
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            UpdateStep::Updating => self.render_updating(frame, area),
            UpdateStep::Confirm => self.render_confirm(frame, area),
            UpdateStep::CommitPush => self.render_commit_push(frame, area),
        }
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        let title_verb = if self.push_only { "Push" } else { "Update" };
        let title_line = Line::from(Span::styled(
            format!("{title_verb} Pull Request #{}?", self.request.number),
            Style::default()
                .fg(colors::BRAND)
                .add_modifier(Modifier::BOLD),
        ));
        let detail_lines = build_detail_lines(&self.request);
        let steps_lines = build_steps_lines(
            self.request.base_ref.as_deref().unwrap_or("?"),
            self.push_only,
        );

        let confirm_height: u16 = 12;
        let detail_height = detail_lines.len() as u16;
        let steps_height = steps_lines.len() as u16;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),              // title
                Constraint::Length(1),              // blank
                Constraint::Length(detail_height),  // labeled rows
                Constraint::Length(1),              // blank
                Constraint::Length(steps_height),   // steps preview
                Constraint::Length(1),              // blank
                Constraint::Length(confirm_height), // ConfirmationModal
                Constraint::Min(0),
            ])
            .split(area);

        frame.render_widget(Paragraph::new(title_line), chunks[0]);
        frame.render_widget(Paragraph::new(detail_lines), chunks[2]);
        frame.render_widget(Paragraph::new(steps_lines), chunks[4]);
        if let Some(dialog) = self.confirm.as_ref() {
            dialog.render(frame, chunks[6]);
        }
    }
}

fn build_confirm(request: &UpdatePullRequestRequest, push_only: bool) -> ConfirmationModal {
    let (title, prompt) = if push_only {
        (
            format!("Push Pull Request #{}", request.number),
            format!("Push branch `{}` to origin?", request.branch),
        )
    } else {
        let base = request.base_ref.as_deref().unwrap_or("base");
        (
            format!("Update Pull Request #{}", request.number),
            format!(
                "Merge `{base}` into branch `{}` and push the update?",
                request.branch
            ),
        )
    };
    ConfirmationModal::new()
        .with_title(title)
        .with_subtitle(prompt)
        .with_confirm_text("Yes")
        .with_cancel_text("No")
        .with_color_value(colors::INFO)
        .with_selected(ConfirmationChoice::Cancel)
}

impl UpdatePullRequestScreen {
    fn render_commit_push(&mut self, frame: &mut Frame, area: Rect) {
        if self.commit_push_done {
            self.render_commit_push_done(frame, area);
            return;
        }
        // Running: PTY panel fills the area; scroll hint sits at the bottom.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),    // Commit & Push Activity panel
                Constraint::Length(1), // shortcuts hint
            ])
            .split(area);
        self.render_commit_push_panel(frame, chunks[0]);
        self.render_commit_push_shortcuts(frame, chunks[1]);
    }

    fn render_commit_push_panel(&mut self, frame: &mut Frame, area: Rect) {
        let panel_label = if self.local_only {
            "Commit Activity"
        } else {
            "Commit & Push Activity"
        };
        let title = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                panel_label,
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::INFO))
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height == 0 || inner.width == 0 {
            return;
        }

        if let Some(pty) = self.pty.as_mut() {
            pty.resize(inner.height, inner.width);
            pty.render(frame, inner);
            render_pty_scrollbar(frame, inner, pty);
            return;
        }

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Waiting for shell...",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ))),
            inner,
        );
    }

    fn render_commit_push_shortcuts(&self, frame: &mut Frame, area: Rect) {
        let muted = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        let separator = Span::styled("  ·  ", muted);
        let mut spans: Vec<Span<'static>> = Vec::new();
        if self.pty.is_some() {
            spans.push(Span::styled(
                "Scroll: ",
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled("↑/↓", Style::default().fg(colors::BRAND)));
            spans.push(Span::styled(" line", muted));
            spans.push(separator.clone());
            spans.push(Span::styled(
                "PgUp/PgDn",
                Style::default().fg(colors::BRAND),
            ));
            spans.push(Span::styled(" page", muted));
            spans.push(separator.clone());
            spans.push(Span::styled("wheel", Style::default().fg(colors::BRAND)));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_commit_push_done(&self, frame: &mut Frame, area: Rect) {
        let (status, headline) = if self.commit_push_succeeded {
            (
                Status::Success,
                if self.local_only {
                    "AI resolution committed successfully!"
                } else {
                    "AI resolution committed and pushed successfully!"
                },
            )
        } else {
            (
                Status::Error,
                if self.local_only {
                    "Commit failed — check the terminal output for details."
                } else {
                    "Commit or push failed — check the terminal output for details."
                },
            )
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // StatusIndicator banner
                Constraint::Min(3),    // summary table
                Constraint::Length(1), // press-any-key hint
            ])
            .split(area);

        StatusIndicator::new(status, headline)
            .without_spinner()
            .render(frame, chunks[0]);

        render_summary_table(&self.commit_push_summary, frame, chunks[1]);

        frame.render_widget(
            Paragraph::new("Press any key to return to dashboard").style(
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
            chunks[2],
        );
    }
}

impl UpdatePullRequestScreen {
    fn render_updating(&mut self, frame: &mut Frame, area: Rect) {
        self.ai_button_rects.set([Rect::default(); 2]);
        self.terminal_button_rects.set([Rect::default(); 2]);
        // The Terminal Activity recovery panel takes over the whole step
        // once a push has failed.
        if self.terminal_active {
            self.render_terminal(frame, area);
            return;
        }
        // Pre-conflict (or "no conflict at all") runs render as just a
        // spinner — the AI Activity panel is reserved for the post-
        // `ConflictsDetected` portion of the pipeline. We also fall back
        // to the spinner-only layout when the area is too short to split.
        if !self.ai_active || area.height < 5 {
            StatusIndicator::new(Status::Loading, self.phase_message.clone())
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }

        let mut constraints = vec![
            Constraint::Length(1), // spinner line
            Constraint::Length(1), // blank
            Constraint::Min(3),    // AI Activity panel
            Constraint::Length(1), // focus / shortcuts hint
        ];
        if self.ai_done {
            constraints.push(Constraint::Length(1)); // blank
            constraints.push(Constraint::Length(3)); // button row
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        StatusIndicator::new(Status::Loading, self.phase_message.clone())
            .with_tick(self.tick)
            .render(frame, chunks[0]);
        self.render_ai_activity(frame, chunks[2]);
        self.render_ai_shortcuts(frame, chunks[3]);
        if self.ai_done {
            self.render_ai_buttons(frame, chunks[5]);
        }
        // The finalize-confirmation modal is an overlay — render it last
        // so it draws over the AI Activity panel without disturbing the
        // PTY emulator state underneath. The PTY child keeps streaming;
        // we just stop forwarding keystrokes to it while the modal owns
        // input.
        if let Some(modal) = self.finalize_confirm.as_ref() {
            modal.render(frame, area);
        }
    }

    fn render_ai_activity(&mut self, frame: &mut Frame, area: Rect) {
        let pty_alive = self.pty.is_some() && !self.ai_done;
        let focused_inner = pty_alive && self.pty_focused;
        let mut title_spans = vec![
            Span::raw(" "),
            Span::styled(
                "AI Activity",
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if pty_alive {
            title_spans.push(Span::styled(
                " · ".to_string(),
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ));
            title_spans.push(Span::styled(
                if focused_inner {
                    "inner focused"
                } else {
                    "outer focused"
                }
                .to_string(),
                Style::default()
                    .fg(if focused_inner {
                        colors::ACCENT
                    } else {
                        colors::INFO
                    })
                    .add_modifier(Modifier::BOLD),
            ));
        }
        title_spans.push(Span::raw(" "));
        let title = Line::from(title_spans);
        let border_color = if focused_inner {
            colors::ACCENT
        } else {
            colors::INFO
        };
        // Only the color flips on focus — keep the border style otherwise
        // untouched so `BorderType::Rounded` glyphs stay rounded. Adding
        // BOLD here makes some terminals swap in a heavier, non-rounded
        // glyph and the rectangle visibly changes shape on Tab.
        let border_style = Style::default().fg(border_color);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border_style)
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height == 0 || inner.width == 0 {
            return;
        }

        // Real opencode embed: keep the vt100 emulator sized to the
        // visible panel area so line wrapping matches what the user
        // sees, then blit its screen state into the panel.
        if let Some(pty) = self.pty.as_mut() {
            pty.resize(inner.height, inner.width);
            pty.render(frame, inner);
            render_pty_scrollbar(frame, inner, pty);
            return;
        }

        // Fallback path — no PTY yet (typically the brief window between
        // "AI active" and the App invoking `spawn_opencode_pty`, or the
        // unit-test renderer). Show a placeholder + any structured
        // events we accumulated (e.g. spawn errors via `Notice`).
        let lines: Vec<Line<'static>> = if self.ai_log.is_empty() {
            vec![Line::from(Span::styled(
                "Waiting for AI to start working on the conflicts...",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ))]
        } else {
            let visible_rows = inner.height as usize;
            let max_scroll = self.ai_log.len().saturating_sub(visible_rows) as u16;
            let scroll = self.ai_scroll.min(max_scroll) as usize;
            let end = self.ai_log.len().saturating_sub(scroll);
            let start = end.saturating_sub(visible_rows);
            self.ai_log[start..end]
                .iter()
                .map(ai_activity_event_to_line)
                .collect()
        };
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_ai_shortcuts(&self, frame: &mut Frame, area: Rect) {
        let muted = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        let separator = Span::styled("  ·  ".to_string(), muted);

        let mut spans: Vec<Span<'static>> = Vec::new();

        if self.ai_done {
            // Post-resolution: Complete / Cancel buttons are the only
            // affordances. Match the dashboard's shortcut-row idiom so
            // the user sees a familiar key reference.
            spans.push(Span::styled(
                "← → ".to_string(),
                Style::default().fg(colors::INFO),
            ));
            spans.push(Span::styled("Switch button".to_string(), muted));
            spans.push(separator.clone());
            spans.push(Span::styled(
                "↵ ".to_string(),
                Style::default().fg(colors::SUCCESS),
            ));
            spans.push(Span::styled("Confirm".to_string(), muted));
            spans.push(separator.clone());
            spans.push(Span::styled(
                "Esc ".to_string(),
                Style::default().fg(colors::ERROR),
            ));
            spans.push(Span::styled("Cancel".to_string(), muted));
            self.append_scroll_hint(&mut spans, &separator, muted);
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }

        // Streaming phase. Show what Tab does and which terminal owns
        // the keyboard right now, in the palette already used by the
        // Dashboard's shortcuts row.
        let focused_inner = self.pty.is_some() && self.pty_focused;
        let focus_label = if focused_inner {
            "Inner (opencode)"
        } else {
            "Outer (wisetree)"
        };
        spans.push(Span::styled("Focus: ".to_string(), muted));
        spans.push(Span::styled(
            focus_label.to_string(),
            Style::default()
                .fg(if focused_inner {
                    colors::ACCENT
                } else {
                    colors::INFO
                })
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(separator.clone());
        spans.push(Span::styled(
            "Tab ".to_string(),
            Style::default().fg(colors::BRAND),
        ));
        spans.push(Span::styled(
            if focused_inner {
                "Switch to Wisetree"
            } else {
                "Switch to opencode"
            }
            .to_string(),
            muted,
        ));
        if focused_inner {
            spans.push(separator.clone());
            spans.push(Span::styled(
                "keys flow into the inner TUI".to_string(),
                Style::default()
                    .fg(colors::GRAY_LIGHT)
                    .add_modifier(Modifier::DIM | Modifier::ITALIC),
            ));
        } else {
            self.append_scroll_hint(&mut spans, &separator, muted);
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Append the scroll-shortcut hint to the footer line. Only shown
    /// when scrolling actually applies — i.e. there's a PTY alive or
    /// the fallback ai_log has content. Colors come from
    /// `design/pallete.md`: the `Scroll:` label is teal/INFO, the key
    /// glyphs are purple/BRAND to mirror the existing `Tab` cue, and
    /// the descriptive words stay muted/dim.
    fn append_scroll_hint(
        &self,
        spans: &mut Vec<Span<'static>>,
        separator: &Span<'static>,
        muted: Style,
    ) {
        let scrollable = self.pty.is_some() || !self.ai_log.is_empty();
        if !scrollable {
            return;
        }
        spans.push(separator.clone());
        spans.push(Span::styled(
            "Scroll: ".to_string(),
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            "↑/↓".to_string(),
            Style::default().fg(colors::BRAND),
        ));
        spans.push(Span::styled(" line".to_string(), muted));
        spans.push(Span::raw(" ".to_string()));
        spans.push(Span::styled(
            "PgUp/PgDn".to_string(),
            Style::default().fg(colors::BRAND),
        ));
        spans.push(Span::styled(" page".to_string(), muted));
        spans.push(Span::raw(" ".to_string()));
        spans.push(Span::styled(
            "Home/End".to_string(),
            Style::default().fg(colors::BRAND),
        ));
        spans.push(Span::styled(" jump".to_string(), muted));
        spans.push(Span::raw(" ".to_string()));
        spans.push(Span::styled(
            "wheel".to_string(),
            Style::default().fg(colors::BRAND),
        ));
    }

    fn render_ai_buttons(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(16),
                Constraint::Length(2),
                Constraint::Length(16),
                Constraint::Min(0),
            ])
            .split(area);

        frame.render_widget(
            button_paragraph(
                "  Apply   ",
                colors::SUCCESS,
                matches!(self.ai_button, AiButton::Complete),
            ),
            chunks[1],
        );
        frame.render_widget(
            button_paragraph(
                "  Cancel  ",
                colors::ERROR,
                matches!(self.ai_button, AiButton::Cancel),
            ),
            chunks[3],
        );
        self.ai_button_rects.set([chunks[1], chunks[3]]);
    }

    /// Render the Terminal Activity recovery layout: a bordered "Push error"
    /// box (sized to the wrapped error so nothing is truncated), the embedded
    /// shell panel, a shortcuts row, and the Accept/Discard decision buttons.
    fn render_terminal(&mut self, frame: &mut Frame, area: Rect) {
        if area.height < 7 {
            StatusIndicator::new(Status::Loading, UPDATE_PUSH_FAILED_MESSAGE)
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        let header_h = self.terminal_header_height(area.width);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header_h), // Push error box
                Constraint::Length(1),        // blank
                Constraint::Min(3),           // Terminal Activity panel
                Constraint::Length(1),        // shortcuts hint
                Constraint::Length(1),        // blank
                Constraint::Length(3),        // Accept / Discard buttons
            ])
            .split(area);

        self.render_terminal_header(frame, chunks[0]);
        self.render_terminal_panel(frame, chunks[2]);
        self.render_terminal_shortcuts(frame, chunks[3]);
        self.render_terminal_buttons(frame, chunks[5]);
    }

    /// Height (borders included) for the "Push error" box: sized to the
    /// wrapped error text and capped so a long git error never crowds out the
    /// shell below it.
    fn terminal_header_height(&self, width: u16) -> u16 {
        const MAX_CONTENT_LINES: usize = 6;
        let inner = width.saturating_sub(2).max(1) as usize;
        let content = terminal_error_lines(self.terminal_header_text())
            .iter()
            .map(|line| line.chars().count().max(1).div_ceil(inner))
            .sum::<usize>()
            .clamp(1, MAX_CONTENT_LINES);
        content as u16 + 2
    }

    fn terminal_header_text(&self) -> &str {
        if self.terminal_error.is_empty() {
            UPDATE_PUSH_FAILED_MESSAGE
        } else {
            self.terminal_error.as_str()
        }
    }

    fn render_terminal_header(&self, frame: &mut Frame, area: Rect) {
        let lines: Vec<Line<'static>> = terminal_error_lines(self.terminal_header_text())
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(colors::ERROR))))
            .collect();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::ERROR))
            .title(Line::from(Span::styled(
                " Push error ".to_string(),
                Style::default()
                    .fg(colors::ERROR)
                    .add_modifier(Modifier::BOLD),
            )));
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_terminal_panel(&mut self, frame: &mut Frame, area: Rect) {
        let pty_alive = self.pty.is_some();
        let focused_inner = pty_alive && self.pty_focused;
        let mut title_spans = vec![
            Span::raw(" "),
            Span::styled(
                "Terminal Activity",
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if pty_alive {
            title_spans.push(Span::styled(
                " · ".to_string(),
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ));
            title_spans.push(Span::styled(
                if focused_inner {
                    "inner focused"
                } else {
                    "outer focused"
                }
                .to_string(),
                Style::default()
                    .fg(if focused_inner {
                        colors::ACCENT
                    } else {
                        colors::INFO
                    })
                    .add_modifier(Modifier::BOLD),
            ));
        }
        title_spans.push(Span::raw(" "));
        let border_color = if focused_inner {
            colors::ACCENT
        } else {
            colors::INFO
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(Line::from(title_spans));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height == 0 || inner.width == 0 {
            return;
        }

        if let Some(pty) = self.pty.as_mut() {
            pty.resize(inner.height, inner.width);
            pty.render(frame, inner);
            render_pty_scrollbar(frame, inner, pty);
            return;
        }

        // No shell (spawn failed) — explain and let the user Discard out.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Could not open a shell here. Press Discard to return to the dashboard.",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ))),
            inner,
        );
    }

    fn render_terminal_shortcuts(&self, frame: &mut Frame, area: Rect) {
        let muted = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        let separator = Span::styled("  ·  ".to_string(), muted);
        let mut spans: Vec<Span<'static>> = Vec::new();

        let focused_inner = self.pty.is_some() && self.pty_focused;
        let focus_label = if focused_inner {
            "Inner (shell)"
        } else {
            "Outer (wisetree)"
        };
        spans.push(Span::styled("Focus: ".to_string(), muted));
        spans.push(Span::styled(
            focus_label.to_string(),
            Style::default()
                .fg(if focused_inner {
                    colors::ACCENT
                } else {
                    colors::INFO
                })
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(separator.clone());
        spans.push(Span::styled(
            "Tab ".to_string(),
            Style::default().fg(colors::BRAND),
        ));
        spans.push(Span::styled(
            if focused_inner {
                "Switch to Wisetree"
            } else {
                "Switch to shell"
            }
            .to_string(),
            muted,
        ));
        if focused_inner {
            spans.push(separator.clone());
            spans.push(Span::styled(
                "keys flow into the shell".to_string(),
                Style::default()
                    .fg(colors::GRAY_LIGHT)
                    .add_modifier(Modifier::DIM | Modifier::ITALIC),
            ));
        } else {
            spans.push(separator.clone());
            spans.push(Span::styled(
                "← → ".to_string(),
                Style::default().fg(colors::INFO),
            ));
            spans.push(Span::styled("Switch button".to_string(), muted));
            spans.push(separator.clone());
            spans.push(Span::styled(
                "↵ ".to_string(),
                Style::default().fg(colors::SUCCESS),
            ));
            spans.push(Span::styled("Confirm".to_string(), muted));
            spans.push(separator.clone());
            spans.push(Span::styled(
                "Esc ".to_string(),
                Style::default().fg(colors::ERROR),
            ));
            spans.push(Span::styled("Discard".to_string(), muted));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_terminal_buttons(&self, frame: &mut Frame, area: Rect) {
        // Each button hugs its own label instead of sharing a fixed width, so
        // the short "Discard" doesn't render as a wide, mostly-empty box. Width
        // = label + 2 borders + 2 padding cells each side; both labels are
        // odd-length, so the even padding keeps the text centered with equal
        // space on both sides (bare labels — `button_paragraph` centers, so
        // any manual padding would just push the text off-center).
        let accept = "Accept & Push";
        let discard = "Discard";
        let button_width = |label: &str| label.len() as u16 + 6;
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(button_width(accept)),
                Constraint::Length(2),
                Constraint::Length(button_width(discard)),
                Constraint::Min(0),
            ])
            .split(area);

        frame.render_widget(
            button_paragraph(
                accept,
                colors::SUCCESS,
                matches!(self.terminal_button, TermButton::Accept),
            ),
            chunks[1],
        );
        frame.render_widget(
            button_paragraph(
                discard,
                colors::ERROR,
                matches!(self.terminal_button, TermButton::Discard),
            ),
            chunks[3],
        );
        self.terminal_button_rects.set([chunks[1], chunks[3]]);
    }
}

/// Split a (possibly multi-line) push error into trimmed, non-empty display
/// lines for the "Push error" box. git writes its push rejection across
/// several lines (`To <remote>` / `! [rejected] ...` / `error: ...` /
/// `hint: ...`), and `run_command` keeps those newlines, so this preserves
/// the structure instead of mashing it onto one truncated row.
fn terminal_error_lines(text: &str) -> Vec<String> {
    let lines: Vec<String> = text
        .lines()
        .map(|line| line.trim_end().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        vec![text.trim().to_string()]
    } else {
        lines
    }
}

/// Render the vertical scrollbar for an embedded PTY that keeps a real vt100
/// scrollback buffer. No-op when there's no scrollback yet.
///
/// vt100's offset is "rows back from the live tail" (0 = bottom). ratatui's
/// scrollbar, however, lands the thumb flush at the bottom of the track only
/// when `position == content_length - 1`. The intuitive mapping is therefore
/// `content_length = scrollback_len + 1` with `position = scrollback_len -
/// offset`: at the live tail (offset 0) `position` hits that maximum so the
/// thumb sits exactly at the bottom. (The earlier `scrollback_len + height`
/// content length left the thumb floating `height - 1` rows short of the
/// bottom even when fully scrolled down.) `viewport_content_length = height`
/// keeps the thumb sized to the visible fraction of the content.
fn render_pty_scrollbar(frame: &mut Frame, inner: Rect, pty: &PtyView) {
    // vt100's offset is "rows back from the live tail" (0 = bottom), which is
    // exactly the tail-anchored model the shared scrollbar expects.
    crate::tui::widgets::render_vertical_scrollbar(
        frame,
        inner,
        pty.scrollback_len(),
        pty.scrollback_offset(),
    );
}

pub(crate) fn contains_position(area: Rect, position: Position) -> bool {
    position.x >= area.left()
        && position.x < area.right()
        && position.y >= area.top()
        && position.y < area.bottom()
}

pub(crate) fn button_paragraph(
    label: &str,
    color: ratatui::style::Color,
    focused: bool,
) -> Paragraph<'static> {
    // Match the canonical `ConfirmationModal` button style: the border
    // only changes color on selection (never gains BOLD), and the label
    // is what actually highlights — that way `BorderType::Rounded`
    // renders identical glyphs whether the button is selected or not.
    let border_color = if focused { color } else { colors::MUTED };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));
    let label_style = if focused {
        Style::default()
            .fg(colors::WHITE)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(colors::MUTED)
    };
    Paragraph::new(Line::from(Span::styled(label.to_string(), label_style)))
        .block(block)
        .alignment(ratatui::layout::Alignment::Center)
}

pub(crate) fn ai_activity_event_to_line(event: &AiActivityEvent) -> Line<'static> {
    match event {
        AiActivityEvent::SessionStart { model } => Line::from(vec![
            Span::styled("[session started] ".to_string(), muted_bold()),
            Span::styled("model".to_string(), muted_dim()),
            Span::styled(": ".to_string(), muted_dim()),
            Span::styled(
                model.clone(),
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        AiActivityEvent::AssistantText { content } => Line::from(vec![
            Span::styled(
                "AI".to_string(),
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": ".to_string(), muted_dim()),
            Span::styled(content.clone(), Style::default().fg(colors::WHITE)),
        ]),
        AiActivityEvent::Thinking { content } => Line::from(vec![
            Span::styled(
                "Thinking".to_string(),
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD | Modifier::ITALIC),
            ),
            Span::styled(": ".to_string(), muted_dim()),
            Span::styled(
                content.clone(),
                Style::default()
                    .fg(colors::GRAY_LIGHT)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]),
        AiActivityEvent::ToolCall { tool_name, summary } => {
            let mut spans = vec![
                Span::styled("> ".to_string(), Style::default().fg(colors::INFO)),
                Span::styled(
                    tool_name.clone(),
                    Style::default()
                        .fg(colors::SUCCESS)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("(".to_string(), muted_dim()),
            ];
            push_highlighted_fragment(&mut spans, summary);
            spans.push(Span::styled(")".to_string(), muted_dim()));
            Line::from(spans)
        }
        AiActivityEvent::ToolResult {
            tool_name,
            status,
            detail,
        } => {
            let (label, label_style) = match status {
                AiToolResultStatus::Success => (
                    "ok",
                    Style::default()
                        .fg(colors::SUCCESS)
                        .add_modifier(Modifier::BOLD),
                ),
                AiToolResultStatus::Error => (
                    "error",
                    Style::default()
                        .fg(colors::ERROR)
                        .add_modifier(Modifier::BOLD),
                ),
            };
            let mut spans = vec![Span::styled(
                "< ".to_string(),
                Style::default().fg(match status {
                    AiToolResultStatus::Success => colors::SUCCESS,
                    AiToolResultStatus::Error => colors::ERROR,
                }),
            )];
            if let Some(tool_name) = tool_name {
                spans.push(Span::styled(
                    tool_name.clone(),
                    Style::default()
                        .fg(colors::INFO)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::raw(" ".to_string()));
            }
            spans.push(Span::styled(label.to_string(), label_style));
            spans.push(Span::styled(": ".to_string(), muted_dim()));
            push_highlighted_fragment(&mut spans, detail);
            Line::from(spans)
        }
        AiActivityEvent::Notice { severity, message } => Line::from(vec![
            Span::styled(
                match severity {
                    AiActivitySeverity::Info => "info",
                    AiActivitySeverity::Warning => "warning",
                    AiActivitySeverity::Error => "error",
                }
                .to_string(),
                Style::default()
                    .fg(match severity {
                        AiActivitySeverity::Info => colors::INFO,
                        AiActivitySeverity::Warning => colors::WARNING,
                        AiActivitySeverity::Error => colors::ERROR,
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": ".to_string(), muted_dim()),
            Span::styled(
                message.clone(),
                Style::default().fg(match severity {
                    AiActivitySeverity::Info => colors::WHITE,
                    AiActivitySeverity::Warning => colors::WARNING,
                    AiActivitySeverity::Error => colors::ERROR,
                }),
            ),
        ]),
        AiActivityEvent::Summary {
            tool_calls,
            duration_ms,
            total_tokens,
        } => Line::from(vec![
            Span::styled(
                "[done] ".to_string(),
                Style::default()
                    .fg(colors::SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(tool_calls.to_string(), Style::default().fg(colors::ACCENT)),
            Span::styled(" tools · ".to_string(), muted_dim()),
            Span::styled(
                format!("{:.1}s", *duration_ms as f64 / 1000.0),
                Style::default().fg(colors::ACCENT),
            ),
            Span::styled(" · ".to_string(), muted_dim()),
            Span::styled(
                total_tokens.to_string(),
                Style::default().fg(colors::ACCENT),
            ),
            Span::styled(" tokens".to_string(), muted_dim()),
        ]),
        AiActivityEvent::Raw { text } => Line::from(Span::styled(
            text.clone(),
            Style::default().fg(colors::GRAY_LIGHT),
        )),
    }
}

fn push_highlighted_fragment(spans: &mut Vec<Span<'static>>, fragment: &str) {
    let mut chars = fragment.chars().peekable();
    let mut first_token = true;
    while let Some(ch) = chars.peek().copied() {
        if ch.is_whitespace() {
            let mut ws = String::new();
            while let Some(next) = chars.peek().copied() {
                if !next.is_whitespace() {
                    break;
                }
                ws.push(next);
                chars.next();
            }
            spans.push(Span::raw(ws));
            continue;
        }

        let mut token = String::new();
        let mut quote: Option<char> = None;
        while let Some(next) = chars.peek().copied() {
            if let Some(active_quote) = quote {
                token.push(next);
                chars.next();
                if next == active_quote {
                    quote = None;
                }
                continue;
            }
            if next.is_whitespace() {
                break;
            }
            if matches!(next, '"' | '\'' | '`') {
                quote = Some(next);
            }
            token.push(next);
            chars.next();
        }

        push_highlighted_token(spans, &token, &mut first_token);
    }
}

fn push_highlighted_token(spans: &mut Vec<Span<'static>>, token: &str, first_token: &mut bool) {
    if let Some((lhs, rhs)) = token.split_once('=') {
        if is_assignment_like(lhs) {
            spans.push(Span::styled(
                lhs.to_string(),
                if lhs.starts_with('-') {
                    Style::default().fg(colors::BRAND)
                } else {
                    Style::default().fg(colors::INFO)
                },
            ));
            spans.push(Span::styled("=".to_string(), muted_dim()));
            if !rhs.is_empty() {
                spans.push(Span::styled(
                    rhs.to_string(),
                    classify_token_style(rhs, false),
                ));
            }
            *first_token = false;
            return;
        }
    }

    spans.push(Span::styled(
        token.to_string(),
        classify_token_style(token, *first_token),
    ));
    *first_token = false;
}

fn classify_token_style(token: &str, first_token: bool) -> Style {
    if is_shell_operator(token) {
        return Style::default().fg(colors::INFO);
    }
    if token.starts_with("--") || (token.starts_with('-') && token.len() > 1) {
        return Style::default().fg(colors::BRAND);
    }
    if is_quoted(token) || is_placeholder(token) {
        return Style::default().fg(colors::WARNING);
    }
    if looks_like_url(token) {
        return Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::UNDERLINED);
    }
    if looks_like_path(token) {
        return Style::default().fg(colors::EMPHASIS);
    }
    if looks_like_number(token) {
        return Style::default().fg(colors::ACCENT);
    }
    if first_token {
        return Style::default()
            .fg(colors::SUCCESS)
            .add_modifier(Modifier::BOLD);
    }
    Style::default().fg(colors::WHITE)
}

fn is_assignment_like(token: &str) -> bool {
    token.starts_with('-')
        || token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
}

fn is_shell_operator(token: &str) -> bool {
    matches!(token, "&&" | "||" | "|" | ";" | "->" | "=>")
}

fn is_quoted(token: &str) -> bool {
    token.len() >= 2
        && ((token.starts_with('"') && token.ends_with('"'))
            || (token.starts_with('\'') && token.ends_with('\''))
            || (token.starts_with('`') && token.ends_with('`')))
}

fn is_placeholder(token: &str) -> bool {
    token.starts_with('<') && token.ends_with('>')
}

fn looks_like_url(token: &str) -> bool {
    token.starts_with("http://") || token.starts_with("https://")
}

fn looks_like_path(token: &str) -> bool {
    let trimmed = token
        .trim_matches(|ch: char| matches!(ch, ',' | ':' | ';' | '(' | ')' | '[' | ']' | '{' | '}'));
    if trimmed.starts_with("~/")
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || trimmed.starts_with('/')
        || trimmed.contains('/')
    {
        return true;
    }
    let lower = trimmed.to_lowercase();
    lower.ends_with(".rs")
        || lower.ends_with(".rb")
        || lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".js")
        || lower.ends_with(".json")
        || lower.ends_with(".md")
}

fn looks_like_number(token: &str) -> bool {
    token.chars().any(|ch| ch.is_ascii_digit())
        && token.chars().all(|ch| {
            ch.is_ascii_digit() || matches!(ch, '.' | '%' | ':' | '+' | '-' | '_' | 's' | 'm')
        })
}

fn muted_dim() -> Style {
    Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM)
}

fn muted_bold() -> Style {
    Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::BOLD)
}

fn build_detail_lines(request: &UpdatePullRequestRequest) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();

    rows.push(labeled_line(
        "PR",
        Span::styled(
            format!("#{} ", request.number),
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        ),
        Some(Span::styled(
            "(Open)".to_string(),
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::DIM),
        )),
    ));

    rows.push(labeled_line(
        "Title",
        Span::styled(
            request.title.clone(),
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
        None,
    ));

    rows.push(labeled_line(
        "URL",
        Span::styled(request.url.clone(), Style::default().fg(colors::EMPHASIS)),
        None,
    ));

    rows.push(labeled_line(
        "Branch",
        Span::styled(
            request.branch.clone(),
            Style::default()
                .fg(colors::SUCCESS)
                .add_modifier(Modifier::BOLD),
        ),
        None,
    ));

    rows.push(labeled_line(
        "Worktree",
        Span::styled(
            request.worktree_path.clone(),
            Style::default().fg(colors::EMPHASIS),
        ),
        None,
    ));

    // Base ref only resolves for the update flow; the push-only flow leaves
    // it `None`, so we omit the row rather than show "(resolving...)".
    if let Some(base_ref) = request.base_ref.clone() {
        rows.push(labeled_line(
            "Base ref",
            Span::styled(
                base_ref,
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            None,
        ));
    }

    // A push-only row is, by definition, not behind — skip the alarming
    // "Behind -0" line in that case.
    if request.behind > 0 {
        rows.push(labeled_line(
            "Behind",
            Span::styled(
                format!("-{}", request.behind),
                Style::default()
                    .fg(colors::ERROR)
                    .add_modifier(Modifier::BOLD),
            ),
            None,
        ));
    }

    if request.ahead > 0 {
        rows.push(labeled_line(
            "Ahead",
            Span::styled(
                format!("+{}", request.ahead),
                Style::default().fg(colors::SUCCESS),
            ),
            None,
        ));
    }

    rows
}

fn build_steps_lines(base_ref: &str, push_only: bool) -> Vec<Line<'static>> {
    let header_style = Style::default()
        .fg(colors::INFO)
        .add_modifier(Modifier::BOLD);
    let bullet_style = Style::default().fg(colors::EMPHASIS);
    let muted = Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM);
    let bullet = |cmd: String| {
        Line::from(vec![
            Span::styled("  • ".to_string(), muted),
            Span::styled(cmd, bullet_style),
        ])
    };
    if push_only {
        return vec![
            Line::from(Span::styled("Will run:".to_string(), header_style)),
            bullet("git push origin HEAD".to_string()),
        ];
    }
    vec![
        Line::from(Span::styled("Will run:".to_string(), header_style)),
        bullet("git fetch --all --prune".to_string()),
        bullet(format!("git merge {base_ref}")),
        bullet("on conflict: opencode streams resolution, then Complete/Cancel".to_string()),
        bullet("git push origin HEAD".to_string()),
    ]
}

/// Translate a crossterm `KeyEvent` into the raw byte sequence a terminal
/// would normally write into the PTY for that keystroke. Covers the keys
/// opencode actually cares about (printable chars, control combos, arrow
/// keys, function keys, navigation). Tab is intentionally not mapped —
/// callers reserve it as the focus-toggle shortcut.
pub(crate) fn key_event_to_pty_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let with_alt = |mut bytes: Vec<u8>| -> Vec<u8> {
        if alt {
            let mut out = Vec::with_capacity(bytes.len() + 1);
            out.push(0x1b);
            out.append(&mut bytes);
            out
        } else {
            bytes
        }
    };

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl+letter → standard ASCII control mapping
                // (Ctrl+A=0x01 .. Ctrl+Z=0x1a, Ctrl+@=0, Ctrl+]=0x1d, …).
                let upper = c.to_ascii_uppercase();
                let byte = match upper {
                    'A'..='Z' => Some((upper as u8) - b'A' + 1),
                    '@' => Some(0x00),
                    '[' => Some(0x1b),
                    '\\' => Some(0x1c),
                    ']' => Some(0x1d),
                    '^' => Some(0x1e),
                    '_' => Some(0x1f),
                    ' ' => Some(0x00),
                    _ => None,
                };
                byte.map(|b| with_alt(vec![b]))
            } else {
                let mut buf = [0u8; 4];
                Some(with_alt(c.encode_utf8(&mut buf).as_bytes().to_vec()))
            }
        }
        KeyCode::Enter => Some(with_alt(vec![b'\r'])),
        KeyCode::Backspace => Some(with_alt(vec![0x7f])),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Left => Some(with_alt(b"\x1b[D".to_vec())),
        KeyCode::Right => Some(with_alt(b"\x1b[C".to_vec())),
        KeyCode::Up => Some(with_alt(b"\x1b[A".to_vec())),
        KeyCode::Down => Some(with_alt(b"\x1b[B".to_vec())),
        KeyCode::Home => Some(with_alt(b"\x1b[H".to_vec())),
        KeyCode::End => Some(with_alt(b"\x1b[F".to_vec())),
        KeyCode::PageUp => Some(with_alt(b"\x1b[5~".to_vec())),
        KeyCode::PageDown => Some(with_alt(b"\x1b[6~".to_vec())),
        KeyCode::Delete => Some(with_alt(b"\x1b[3~".to_vec())),
        KeyCode::Insert => Some(with_alt(b"\x1b[2~".to_vec())),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::F(n) => {
            let seq: &[u8] = match n {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => return None,
            };
            Some(seq.to_vec())
        }
        _ => None,
    }
}

/// Build the finalize-confirmation modal that opens when the user
/// presses Enter on the outer focus while opencode is streaming. Yes is
/// preselected so a careful confirmation flow (Enter → modal → Enter)
/// commits the merge resolution; No / Esc returns to the live PTY.
fn build_finalize_modal() -> ConfirmationModal {
    ConfirmationModal::new()
        .with_title("Confirm finalization")
        .with_subtitle("Do you confirm that the merge resolution is finalized?")
        .with_confirm_text("Yes")
        .with_cancel_text("No")
        .with_color("#eada61")
        .with_selected(ConfirmationChoice::Confirm)
}

fn labeled_line(
    label: &str,
    value: Span<'static>,
    trailing: Option<Span<'static>>,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
    spans.push(Span::styled(
        format!("{label:<12}"),
        Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM),
    ));
    spans.push(value);
    if let Some(extra) = trailing {
        spans.push(extra);
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_dump(screen: &mut UpdatePullRequestScreen, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| screen.render(f, f.area())).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn sample_request() -> UpdatePullRequestRequest {
        UpdatePullRequestRequest {
            number: 21,
            title: "Improve onboarding flow".to_string(),
            url: "https://github.com/example/repo/pull/21".to_string(),
            branch: "feat-onboarding".to_string(),
            worktree_path: "/tmp/repo-onboarding".to_string(),
            ahead: 4,
            behind: 7,
            base_ref: None,
        }
    }

    #[test]
    fn screen_starts_in_loading_when_base_ref_unknown() {
        let screen = UpdatePullRequestScreen::new(sample_request());
        assert_eq!(screen.step(), UpdateStep::Loading);
        assert!(screen.error().is_none());
    }

    #[test]
    fn set_base_ref_transitions_to_confirm_default_no() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        assert_eq!(screen.step(), UpdateStep::Confirm);
        let dialog = screen
            .confirm
            .as_ref()
            .expect("confirm built after base ref");
        assert_eq!(dialog.selected(), ConfirmationChoice::Cancel);
    }

    #[test]
    fn enter_on_no_returns_cancelled() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::Cancelled);
    }

    #[test]
    fn tab_then_enter_confirms() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(tab), UpdateAction::Continue);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::Confirmed);
    }

    #[test]
    fn new_local_conflict_starts_in_updating_with_ai_active() {
        let screen = UpdatePullRequestScreen::new_local_conflict(sample_request());
        assert!(screen.local_only());
        assert_eq!(screen.step(), UpdateStep::Updating);
        assert!(screen.ai_active());
    }

    #[test]
    fn local_only_done_page_drops_push_wording() {
        let mut screen = UpdatePullRequestScreen::new_local_conflict(sample_request());
        // Jump to the finished commit page as if the local `git commit`
        // succeeded.
        screen.step = UpdateStep::CommitPush;
        screen.commit_push_succeeded = true;
        screen.commit_push_summary = vec![SummaryRow::success(screen.commit_action_label())];
        screen.commit_push_done = true;

        let dump = render_dump(&mut screen, 80, 12);
        assert!(
            dump.contains("committed successfully"),
            "local done page should confirm the commit: {dump}"
        );
        assert!(
            !dump.to_lowercase().contains("push"),
            "local done page must not mention push: {dump}"
        );
    }

    #[test]
    fn pr_flow_done_page_keeps_push_wording() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.step = UpdateStep::CommitPush;
        screen.commit_push_succeeded = true;
        screen.commit_push_summary = vec![SummaryRow::success(screen.commit_action_label())];
        screen.commit_push_done = true;

        let dump = render_dump(&mut screen, 80, 12);
        assert!(
            dump.to_lowercase().contains("pushed"),
            "PR done page should mention the push: {dump}"
        );
    }

    #[test]
    fn esc_during_loading_cancels() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), UpdateAction::Cancelled);
    }

    #[test]
    fn keys_are_ignored_while_updating_before_ai_done() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), UpdateAction::Continue);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::Continue);
        assert!(screen.is_updating());
    }

    #[test]
    fn set_error_clears_confirm_and_any_key_dismisses() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_error("boom".to_string());
        let any = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(screen.handle_key(any), UpdateAction::Cancelled);
    }

    #[test]
    fn enter_on_complete_returns_ai_complete() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        screen.mark_ai_done();
        assert_eq!(screen.ai_button(), AiButton::Complete);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::AiComplete);
    }

    #[test]
    fn right_then_enter_returns_ai_cancel() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        screen.mark_ai_done();
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(right), UpdateAction::Continue);
        assert_eq!(screen.ai_button(), AiButton::Cancel);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::AiCancel);
    }

    #[test]
    fn mark_ai_done_is_idempotent_and_preserves_button_selection() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        screen.mark_ai_done();
        assert_eq!(screen.ai_button(), AiButton::Complete);

        // User flips to Cancel...
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(right), UpdateAction::Continue);
        assert_eq!(screen.ai_button(), AiButton::Cancel);

        // ...and the next PTY tick re-fires mark_ai_done. The selection
        // must survive — otherwise Cancel is unreachable.
        screen.mark_ai_done();
        assert_eq!(screen.ai_button(), AiButton::Cancel);

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::AiCancel);
    }

    #[test]
    fn esc_after_ai_done_returns_ai_cancel() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        screen.mark_ai_done();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), UpdateAction::AiCancel);
    }

    #[test]
    fn ai_log_truncates_at_cap() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        for i in 0..(AI_LOG_MAX_LINES + 50) {
            screen.append_ai_line(format!("line {i}"));
        }
        let lines = screen.ai_log_lines();
        assert_eq!(lines.len(), AI_LOG_MAX_LINES);
        assert_eq!(
            lines.last().unwrap(),
            &format!("line {}", AI_LOG_MAX_LINES + 49)
        );
    }

    #[test]
    fn assistant_activity_deltas_coalesce_into_one_row() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.append_ai_line(AiActivityEvent::AssistantText {
            content: "git".to_string(),
        });
        screen.append_ai_line(AiActivityEvent::AssistantText {
            content: " status".to_string(),
        });

        assert_eq!(screen.ai_log_lines(), vec!["AI: git status".to_string()]);
    }

    #[test]
    fn tool_call_activity_uses_monokai_style_roles() {
        let line = ai_activity_event_to_line(&AiActivityEvent::ToolCall {
            tool_name: "run_shell_command".to_string(),
            summary: "git diff -- src/main.rs --color=never".to_string(),
        });

        assert!(
            line.spans
                .iter()
                .any(|span| span.style.fg == Some(colors::SUCCESS)),
            "expected command token highlighting: {line:?}"
        );
        assert!(
            line.spans
                .iter()
                .any(|span| span.style.fg == Some(colors::BRAND)),
            "expected flag highlighting: {line:?}"
        );
        assert!(
            line.spans
                .iter()
                .any(|span| span.style.fg == Some(colors::EMPHASIS)),
            "expected path highlighting: {line:?}"
        );
    }

    #[test]
    fn phase_message_updates_during_updating() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.set_phase_message("Conflict found — opencode resolving...");
        assert_eq!(
            screen.phase_message(),
            "Conflict found — opencode resolving..."
        );
    }

    #[test]
    fn mouse_wheel_scrolls_ai_activity_when_ai_active() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();

        assert_eq!(screen.ai_scroll(), 0);
        assert!(screen.handle_mouse_scroll_up(3));
        assert_eq!(screen.ai_scroll(), 3);
        assert!(screen.handle_mouse_scroll_down(2));
        assert_eq!(screen.ai_scroll(), 1);
        assert!(screen.handle_mouse_scroll_down(9));
        assert_eq!(screen.ai_scroll(), 0);
    }

    #[test]
    fn ai_activity_scroll_stays_stable_when_new_lines_arrive() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        for i in 0..10 {
            screen.append_ai_line(format!("line {i}"));
        }

        assert!(screen.handle_mouse_scroll_up(4));
        assert_eq!(screen.ai_scroll(), 4);

        screen.append_ai_line("line 10");
        assert_eq!(screen.ai_scroll(), 5);
    }

    #[test]
    fn ai_activity_panel_hidden_until_marked_active() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        assert!(!screen.ai_active());

        let before = render_dump(&mut screen, 100, 24);
        assert!(
            !before.contains("AI Activity"),
            "AI Activity panel rendered before conflicts detected:\n{before}"
        );

        screen.mark_ai_active();
        assert!(screen.ai_active());
        let after = render_dump(&mut screen, 100, 24);
        assert!(
            after.contains("AI Activity"),
            "AI Activity panel missing after mark_ai_active:\n{after}"
        );
    }

    #[test]
    fn apply_and_cancel_buttons_visible_after_ai_done() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        screen.mark_ai_done();
        assert!(screen.ai_done());
        let dumped = render_dump(&mut screen, 100, 28);
        assert!(dumped.contains("Apply"), "expected Apply button:\n{dumped}");
        assert!(
            dumped.contains("Cancel"),
            "expected Cancel button:\n{dumped}"
        );
    }

    #[test]
    fn updating_height_is_compact_until_ai_active() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        let compact = screen.preferred_content_height();
        screen.mark_ai_active();
        let expanded = screen.preferred_content_height();
        assert!(
            expanded > compact,
            "expected AI-active height ({expanded}) to exceed pre-AI height ({compact})"
        );
    }

    #[test]
    fn start_updating_resets_ai_active_flag() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        assert!(screen.ai_active());
        screen.mark_ai_done();
        assert!(screen.ai_done());
        screen.start_updating();
        assert!(!screen.ai_active());
        assert!(!screen.ai_done());
    }

    #[test]
    fn key_event_to_pty_bytes_maps_common_keys() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let plain = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(key_event_to_pty_bytes(&plain), Some(b"a".to_vec()));

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(key_event_to_pty_bytes(&enter), Some(b"\r".to_vec()));

        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(key_event_to_pty_bytes(&backspace), Some(vec![0x7f]));

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(key_event_to_pty_bytes(&esc), Some(vec![0x1b]));

        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(key_event_to_pty_bytes(&up), Some(b"\x1b[A".to_vec()));

        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(key_event_to_pty_bytes(&ctrl_c), Some(vec![0x03]));

        let alt_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(key_event_to_pty_bytes(&alt_a), Some(vec![0x1b, b'a']));
    }

    #[test]
    fn pty_focus_defaults_to_outer_after_start_updating() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        assert!(!screen.is_pty_focused());
    }

    #[test]
    fn footer_includes_scroll_hint_during_streaming_when_log_has_content() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        screen.append_ai_line("a line of output".to_string());

        let dumped = render_dump(&mut screen, 140, 28);
        assert!(
            dumped.contains("Scroll:"),
            "expected scroll hint in footer:\n{dumped}"
        );
        assert!(
            dumped.contains("PgUp/PgDn"),
            "expected PgUp/PgDn hint in footer:\n{dumped}"
        );
    }

    #[test]
    fn pty_focus_indicator_renders_below_ai_activity_panel() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        let dumped = render_dump(&mut screen, 100, 28);
        // Streaming-phase hint must always mention the Tab shortcut and
        // which terminal currently owns the keyboard.
        assert!(
            dumped.contains("Tab"),
            "expected Tab shortcut hint in:\n{dumped}"
        );
        assert!(
            dumped.contains("Focus"),
            "expected focus indicator in:\n{dumped}"
        );
    }

    #[test]
    fn enter_on_outer_focus_while_streaming_opens_finalize_modal() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        assert!(screen.finalize_confirm().is_none());

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::Continue);
        assert_eq!(screen.finalize_confirm(), Some(ConfirmationChoice::Confirm));
        // Opening the modal must not prematurely finalize anything.
        assert!(!screen.ai_done());
    }

    #[test]
    fn tab_inside_finalize_modal_toggles_selection() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        screen.handle_key(enter);
        assert_eq!(screen.finalize_confirm(), Some(ConfirmationChoice::Confirm));

        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        screen.handle_key(tab);
        assert_eq!(screen.finalize_confirm(), Some(ConfirmationChoice::Cancel));
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        screen.handle_key(right);
        assert_eq!(screen.finalize_confirm(), Some(ConfirmationChoice::Confirm));
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        screen.handle_key(left);
        assert_eq!(screen.finalize_confirm(), Some(ConfirmationChoice::Cancel));
    }

    #[test]
    fn enter_on_yes_in_finalize_modal_transitions_to_review() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        screen.handle_key(enter); // opens modal (Yes selected)

        // Confirm with Enter — must close the modal and surface the
        // Complete/Cancel review buttons.
        let confirm = screen.handle_key(enter);
        assert_eq!(confirm, UpdateAction::Continue);
        assert!(screen.finalize_confirm().is_none());
        assert!(screen.ai_done());
        assert_eq!(screen.ai_button(), AiButton::Complete);
    }

    #[test]
    fn enter_on_no_in_finalize_modal_dismisses_and_keeps_streaming() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        screen.handle_key(enter); // opens modal
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        screen.handle_key(tab); // flips to No
        assert_eq!(screen.finalize_confirm(), Some(ConfirmationChoice::Cancel));

        let confirm = screen.handle_key(enter);
        assert_eq!(confirm, UpdateAction::Continue);
        assert!(screen.finalize_confirm().is_none());
        assert!(
            !screen.ai_done(),
            "No must not flip the screen into the review state"
        );
    }

    #[test]
    fn esc_inside_finalize_modal_dismisses_without_finalizing() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        screen.handle_key(enter);
        assert_eq!(screen.finalize_confirm(), Some(ConfirmationChoice::Confirm));
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        screen.handle_key(esc);
        assert!(screen.finalize_confirm().is_none());
        assert!(!screen.ai_done());
    }

    #[test]
    fn finalize_modal_renders_centered_overlay() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        screen.handle_key(enter);

        let dumped = render_dump(&mut screen, 100, 28);
        assert!(
            dumped.contains("Do you confirm that the merge resolution is finalized?"),
            "expected modal prompt in:\n{dumped}"
        );
        // "  Yes  " in a 7-cell inner area = symmetric 2-cell margins.
        assert!(
            dumped.contains("  Yes  "),
            "expected centered Yes button in:\n{dumped}"
        );
        // "  No  " in a 6-cell inner area = symmetric 2-cell margins.
        assert!(
            dumped.contains("  No  "),
            "expected centered No button in:\n{dumped}"
        );
    }

    #[test]
    fn enter_with_outer_focus_does_not_open_modal_before_ai_active() {
        // Pre-AI Activity (e.g. fetching base ref) — Enter is still a
        // swallowed no-op, not a modal trigger.
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::Continue);
        assert!(screen.finalize_confirm().is_none());
    }

    #[test]
    fn render_confirm_shows_base_ref_and_buttons() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());

        let dumped = render_dump(&mut screen, 100, 28);

        assert!(
            dumped.contains("Update Pull Request #21?"),
            "expected title in:\n{dumped}"
        );
        assert!(dumped.contains("upstream/main"));
        assert!(dumped.contains("-7"));
        assert!(dumped.contains("Yes"));
        assert!(dumped.contains("No"));
    }

    #[test]
    fn new_push_lands_on_confirm_in_push_only_mode() {
        let screen = UpdatePullRequestScreen::new_push(sample_request());
        assert_eq!(screen.step(), UpdateStep::Confirm);
        assert!(screen.is_push_only());
        assert!(screen.error().is_none());
    }

    #[test]
    fn render_push_confirm_shows_push_wording_only() {
        let mut screen = UpdatePullRequestScreen::new_push(sample_request());
        let dumped = render_dump(&mut screen, 100, 24);
        assert!(
            dumped.contains("Push Pull Request #21?"),
            "expected push title in:\n{dumped}"
        );
        assert!(
            dumped.contains("git push origin HEAD"),
            "expected push step in:\n{dumped}"
        );
        // The push-only flow never fetches or merges.
        assert!(
            !dumped.contains("git fetch"),
            "push confirm must not mention fetch:\n{dumped}"
        );
        assert!(
            !dumped.contains("git merge"),
            "push confirm must not mention merge:\n{dumped}"
        );
    }

    #[test]
    fn terminal_mode_grows_preferred_height() {
        let mut screen = UpdatePullRequestScreen::new_push(sample_request());
        screen.enter_terminal_mode_for_test();
        assert!(screen.terminal_active());
        assert!(
            screen.preferred_content_height() >= 25,
            "terminal panel needs room: {}",
            screen.preferred_content_height()
        );
    }

    #[test]
    fn terminal_outer_enter_accepts_and_esc_discards() {
        let mut screen = UpdatePullRequestScreen::new_push(sample_request());
        screen.enter_terminal_mode_for_test();

        // Default focus is Accept → Enter re-pushes.
        assert_eq!(screen.terminal_button(), TermButton::Accept);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::TerminalAccept);

        // Esc always discards regardless of focused button.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), UpdateAction::TerminalDiscard);
    }

    #[test]
    fn terminal_arrows_switch_button_then_enter_discards() {
        let mut screen = UpdatePullRequestScreen::new_push(sample_request());
        screen.enter_terminal_mode_for_test();

        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(right), UpdateAction::Continue);
        assert_eq!(screen.terminal_button(), TermButton::Discard);

        // With Discard focused, Enter discards.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::TerminalDiscard);
    }

    #[test]
    fn render_terminal_panel_shows_terminal_activity_title() {
        let mut screen = UpdatePullRequestScreen::new_push(sample_request());
        screen.enter_terminal_mode_for_test();
        let dumped = render_dump(&mut screen, 100, 28);
        assert!(
            dumped.contains("Terminal Activity"),
            "expected Terminal Activity panel title in:\n{dumped}"
        );
        assert!(
            dumped.contains("Accept") && dumped.contains("Discard"),
            "expected Accept/Discard buttons in:\n{dumped}"
        );
    }

    #[test]
    fn terminal_error_lines_splits_multiline_git_error() {
        let err = "To github.com:me/repo.git\n ! [rejected]   HEAD -> main (fetch first)\nerror: failed to push some refs to 'github.com:me/repo.git'";
        let lines = terminal_error_lines(err);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "To github.com:me/repo.git");
        assert!(lines[2].starts_with("error: failed to push"));
    }

    #[test]
    fn terminal_error_box_shows_full_error_wrapped_not_truncated() {
        let mut screen = UpdatePullRequestScreen::new_push(sample_request());
        // A realistic multi-line git push rejection. A nonexistent shell makes
        // the spawn fail (pty = None) without affecting the error-box render.
        screen.start_terminal_recovery(
            std::path::PathBuf::from("/nonexistent-shell-xyz"),
            Vec::new(),
            std::env::temp_dir(),
            "To github.com:me/repo.git\n ! [rejected] HEAD -> main (fetch first)\n\
             error: failed to push some refs to 'github.com:me/repo.git'"
                .to_string(),
        );
        let dumped = render_dump(&mut screen, 100, 30);
        assert!(
            dumped.contains("Push error"),
            "expected dedicated error box title:\n{dumped}"
        );
        // The tail of the error (previously cut off at "...some re") must now
        // be fully visible.
        assert!(
            dumped.contains("failed to push some refs"),
            "full error must not be truncated:\n{dumped}"
        );
    }

    #[test]
    fn terminal_button_label_is_horizontally_centered() {
        // Render a focused button into a 19-wide cell (matches
        // `render_terminal_buttons`) and assert the label sits with equal
        // padding on both sides — i.e. no asymmetric manual padding sneaks
        // back into the label.
        let backend = TestBackend::new(19, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(
                    button_paragraph("Accept & Push", colors::SUCCESS, true),
                    f.area(),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        // Row 1 sits between the top/bottom borders; cols 0 and 18 are the
        // side borders, so the usable inner span is cols 1..=17.
        let symbols: Vec<String> = (0..19)
            .map(|x| buffer[(x, 1)].symbol().to_string())
            .collect();
        let first_text = (1..18)
            .find(|&x| symbols[x] != " ")
            .expect("label rendered");
        let last_text = (1..18)
            .rev()
            .find(|&x| symbols[x] != " ")
            .expect("label rendered");
        let left_pad = first_text - 1;
        let right_pad = 17 - last_text;
        assert_eq!(
            left_pad, right_pad,
            "button label not centered (left {left_pad} vs right {right_pad}): {symbols:?}"
        );
    }

    #[test]
    fn terminal_tab_toggles_focus_when_shell_alive() {
        // Spawns a real shell so `pty.is_some()` and Tab can toggle focus.
        // Skips cleanly if no shell binary is present in the test env.
        let shell = ["/bin/sh", "/bin/bash"]
            .into_iter()
            .map(std::path::PathBuf::from)
            .find(|p| p.exists());
        let Some(shell) = shell else {
            return;
        };
        let mut screen = UpdatePullRequestScreen::new_push(sample_request());
        screen.start_updating();
        screen.start_terminal_recovery(
            shell,
            Vec::new(),
            std::env::temp_dir(),
            "remote rejected".to_string(),
        );
        assert!(screen.terminal_active());
        assert!(!screen.is_pty_focused());

        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(tab), UpdateAction::Continue);
        assert!(screen.is_pty_focused(), "Tab should focus the inner shell");

        assert_eq!(screen.handle_key(tab), UpdateAction::Continue);
        assert!(
            !screen.is_pty_focused(),
            "Tab again should return focus to Wisetree"
        );
    }
}

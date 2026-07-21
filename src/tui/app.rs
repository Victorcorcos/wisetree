//! `App` — central TUI state machine.
//!
//! Owns screen routing, per-screen async work, and the wrapper-mode selected
//! path handoff used by shell integration.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{env, ffi::OsString};

#[cfg(unix)]
use libc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;
use tokio::sync::mpsc;

use crate::cli::AppMode;
use crate::config::schema::{DashboardConfig, LinkStrategy, NotificationsConfig, WorktreeConfig};
use crate::config::service::ConfigService;
use crate::constants::{global_config_file, LOCAL_CONFIG_FILE_NAME};
use crate::errors::user_friendly_message;
use crate::files::service::{open_terminal, open_url};
use crate::git::exec::get_git_root;
use crate::git::service::GitService;
use crate::git::types::{GitBranch, GitWorktree, WorktreeCreateOptions};
use crate::messages::{colors, CREATE_SUCCESS, DELETE_SUCCESS};
use crate::services::dashboard::{review_feedback_needs_expanded_context, ReviewRevisionMode};
use crate::services::presets::WisePresetDiscovery;
use crate::services::{
    build_review_summary, check_for_updates_all_sources, compute_attempt_changes,
    default_dashboard_warning, detect_shell_integration, fetch_free_opencode_models,
    fetch_opencode_model_variants, fetch_opencode_models, install_shell_integration,
    parse_pull_request_md, resolve_dashboard_columns, AiStatus, AttemptChanges, BugHypothesis,
    BugkillPreflightOutcome, BugkillResumeState, BugkillSnapshot, BugkillVerdict, CheckStatus,
    CommentGroup, DashboardNoticeLevel, DashboardRow, DashboardService, DashboardUpdate,
    DashboardWatch, ExplainPreparation, ExplainSubmitOutcome, ExplainSubmitRequest,
    FixApplyHandoff, FixCommitOutcome, FixPlan, FixPreparation, FixVerdict, JudgeResult,
    MultiSourceUpdateResult, OpencodeModel, OpencodeTurn, OpencodeTurnWatcher, PrState,
    ReviewContext, ReviewFile, ReviewFinding, ReviewPreparation, ReviewScanMode,
    ReviewScanTelemetry, ReviewVerification, Shell, ShellIntegrationStatus, UpdateBranchOutcome,
    UpdatePhase, UpdateProgress, UpdateSource,
};
use crate::tui::event::{Event, EventLoop};
use crate::tui::router::Screen;
use crate::tui::screens;
use crate::tui::screens::ai_model_picker::{AiModelPickerAction, AiModelPickerScreen};
use crate::tui::screens::bugkill_pr::{BugkillAction, BugkillPullRequestScreen};
use crate::tui::screens::cache::{CacheAction as CacheScreenAction, CacheScreen};
use crate::tui::screens::create::{CreateAction, CreateScreen};
use crate::tui::screens::dashboard::{
    BugkillRequest, BulkDeleteStatus, ClosePullRequestRequest, DashboardAction, DashboardScreen,
    ExplainPullRequestRequest, FixPullRequestRequest, MergePullRequestRequest,
    ReviewPullRequestRequest, UpdatePullRequestRequest,
};
use crate::tui::screens::delete::{
    DeleteAction, DeleteOutcome as ScreenDeleteOutcome, DeleteScreen, DeleteStep,
};
use crate::tui::screens::explain_pr::{ExplainAction, ExplainPullRequestScreen, ExplainStep};
use crate::tui::screens::fix_pr::{FixAction, FixPullRequestScreen, FixRowOutcome, FixStep};
use crate::tui::screens::menu::{MenuChoice, MenuOutcome, MenuScreen};
use crate::tui::screens::merge_pr::{MergeAction, MergePullRequestScreen, MergeStep};
#[cfg(test)]
use crate::tui::screens::review_pr::COVERAGE_SCAN_INDEX;
use crate::tui::screens::review_pr::{ReviewAction, ReviewPullRequestScreen, ReviewRowOutcome};
use crate::tui::screens::settings::{
    CopyDirection, SettingsAction, SettingsScreen, SettingsStep, UpgradeOutcome,
};
use crate::tui::screens::setup::{SetupAction, SetupScreen, SetupStep};
use crate::tui::screens::setup_project::{
    SetupProjectAction, SetupProjectPresetValues, SetupProjectScreen, SetupProjectStep,
};
use crate::tui::screens::update_branch::UpdateBranchScreen;
use crate::tui::screens::update_pr::{UpdateAction, UpdatePullRequestScreen, UpdateStep};
use crate::tui::selection::{
    clamp_position, contains_position, extract_text, MouseSelection, SelectionOverlay,
};
use crate::tui::terminal;
use crate::tui::widgets::SummaryRow;
use crate::tui::widgets::{render_toast, ToastState, ToastVariant, WelcomeHeader};
use crate::utils::path::{repository_base_name, TemplateVariables};
use crate::worktree::service::{
    CreateOutcome as ServiceCreateOutcome, DeleteOutcome as ServiceDeleteOutcome,
};
use crate::worktree::WorktreeService;
use crate::VERSION;

const SETTINGS_PATH_COPIED_MESSAGE: &str =
    "Setting file copied to Clipboard, edit it with your favorite editor!";

/// Lines a single mouse-wheel tick advances a scrollable panel by.
/// Matches the common browser default (3) so the diff feels familiar.
const WHEEL_LINES_PER_TICK: u16 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitPhase {
    Loading,
    Ready,
    Errored,
}

enum AppEvent {
    Initialized(Box<InitOutcome>),
    CacheLoaded(Result<crate::files::CacheOverview, String>),
    CacheEntryDeleted(Result<crate::files::CacheOverview, String>),
    CreateBranchesLoaded(Result<Vec<GitBranch>, String>),
    CreateFinished(Result<ServiceCreateOutcome, String>),
    /// One line of activity from the create pipeline — stage banner, stdout, or
    /// stderr. Routed into the Terminal Activity panel under the "Creating"
    /// step so long-running post-create commands (`flutter pub get`,
    /// `bun install`) surface their output live instead of after they finish.
    CreateActivity {
        text: String,
        kind: crate::files::ActivityKind,
    },
    DeleteLoaded(Result<Vec<GitWorktree>, String>),
    DeleteFinished(Result<ServiceDeleteOutcome, String>),
    SettingsUpdateChecked(MultiSourceUpdateResult),
    SettingsUpgradeFinished {
        source: UpdateSource,
        result: Result<String, String>,
    },
    SetupInstalled(Result<ShellIntegrationStatus, String>),
    ClipboardCopyFinished {
        success_message: String,
        error: Option<String>,
    },
    WisePresetDiscovered(Result<WisePresetDiscovery, String>),
    MergePrDetailsLoaded(Result<MergePrDetailsPayload, String>),
    MergePrFinished(Result<u64, MergePrFailure>),
    ClosePrFinished(Result<u64, String>),
    UpdatePrBaseRefResolved {
        number: u64,
        base_ref: Option<String>,
    },
    /// Live progress signal from the update-PR pipeline. Drives both the
    /// granular phase toasts and the AI activity panel inside the
    /// UpdatePullRequestScreen.
    UpdatePrProgress {
        number: u64,
        progress: UpdateProgress,
    },
    UpdatePrFinished(Result<UpdatePrSuccess, UpdatePrFailure>),
    UpdateBranchFinished(Result<UpdateBranchOutcome, String>),
    /// Base ref resolved for the "Explain Pull Request" flow.
    ExplainPrBaseRefResolved {
        base_ref: Option<String>,
    },
    /// Read-only preparation finished — either the opencode spawn params
    /// (`HandedOffToUi`) or a terminal non-handoff variant.
    ExplainPrPrepared(Result<Box<ExplainPreparation>, String>),
    /// The drafted PR was submitted (created or updated).
    ExplainPrSubmitted(Result<ExplainSubmitOutcome, String>),
    /// A line of terminal output from the git push / gh pr create pipeline.
    /// Routed into the Terminal Activity panel under the Opening step.
    ExplainPrActivity {
        text: String,
        kind: crate::files::ActivityKind,
    },
    /// "Fix Pull Request": sync + fetch + group review comments finished.
    FixPrPrepared(Result<Box<FixPreparation>, String>),
    /// One comment group's captured planning call finished. `index` lets the
    /// handler ignore a result that arrives after the user moved on. `is_replan`
    /// is true for the "Other" path (feedback was supplied), so the handler
    /// always returns to the Decision screen with a revised plan rather than
    /// acting on a `reply` / `praise` verdict and skipping the user's approval.
    FixPrPlanned {
        index: usize,
        is_replan: bool,
        result: Result<FixVerdict, String>,
    },
    /// A non-actionable reply was posted (the `reply` verdict).
    FixPrReplied {
        index: usize,
        result: Result<(), String>,
    },
    /// Apply spawn params are ready — spawn opencode into the AI panel.
    FixPrApplyReady {
        index: usize,
        result: Result<Box<FixApplyHandoff>, String>,
    },
    /// "Review Pull Request": sync + diff fetch + per-file split finished.
    ReviewPrPrepared(Result<Box<ReviewPreparation>, String>),
    /// One per-file scan (or a single-finding revision) returned.
    ReviewPrScanned {
        /// File index the scan belongs to — guards against a stale result.
        file_index: usize,
        retry: ReviewScanRetry,
        result: Result<Vec<ReviewFinding>, String>,
        telemetry: Option<ReviewScanTelemetry>,
        raw_output: Option<String>,
    },
    /// An "Other" revision of the current finding returned.
    ReviewPrRevised {
        index: usize,
        mode: ReviewRevisionMode,
        feedback: String,
        result: Result<Vec<ReviewFinding>, String>,
        telemetry: Option<ReviewScanTelemetry>,
    },
    ReviewPrVerified {
        index: usize,
        result: Result<ReviewVerification, String>,
        telemetry: Option<ReviewScanTelemetry>,
    },
    ReviewPrGapAudited {
        result: Result<Vec<ReviewFinding>, String>,
        telemetry: Option<ReviewScanTelemetry>,
    },
    /// One approved finding was posted (or failed to post) on the PR.
    ReviewPrPosted {
        index: usize,
        result: Result<(), String>,
    },
    /// The review summary submission finished.
    ReviewPrSummarySubmitted {
        request_changes: bool,
        result: Result<(), String>,
    },
    /// A fix apply finished: either committed + replied, or no change was
    /// needed and the reviewer was told it's already addressed.
    FixPrCommitted {
        index: usize,
        result: Result<FixCommitOutcome, String>,
    },
    /// The final `git push` finished; show the results page.
    FixPrPushed(Result<(), String>),
    /// "Bugkill": deterministic preflight finished (gates, clean-tree check,
    /// untracked baseline, base ref, resume detection).
    BugkillPrepared(Result<Box<BugkillPreflightOutcome>, String>),
    /// Leftover-attempt debris was discarded — re-run the preflight.
    BugkillDiscarded(Result<(), String>),
    /// Spawn params for the live investigation `opencode run` are ready
    /// (or a gate failed). `corrective` marks the single retry after a
    /// parse failure.
    BugkillInvestigateReady {
        corrective: bool,
        result: Result<Box<FixApplyHandoff>, String>,
    },
    /// Pre-attempt snapshot taken and opencode spawn params ready.
    BugkillFixReady {
        row_index: usize,
        result: Result<Box<(BugkillSnapshot, FixApplyHandoff)>, String>,
    },
    /// Post-attempt scan + harness commit finished.
    BugkillCommitted(Result<BugkillCommitOutcome, String>),
    /// Esc-abort cleanup finished (uncommitted partial edits discarded).
    BugkillAborted(Result<(), String>),
    /// The judge classified the user's freeform "Other" answer.
    BugkillJudged {
        user_text: String,
        result: Result<BugkillVerdict, String>,
    },
    /// `git revert` of the attempt commit finished.
    BugkillRolledBack(Result<(), String>),
    /// Rewriting `BUG_INVESTIGATION.md` failed (best-effort warning).
    BugkillFileWriteFailed(String),
    /// Result of the background fetch that powers the AI provider/model
    /// picker. The picker stays in its loading state until this lands.
    AiModelsFetched(Result<Vec<OpencodeModel>, String>),
    /// Result of the background `opencode models opencode` shell-out that
    /// powers the AI Settings free-model quick-pick chip row.
    FreeOpencodeModelsFetched(Result<Vec<String>, String>),
    /// Result of the background `opencode models --verbose` shell-out that maps
    /// each `provider/model` to its authoritative reasoning variants, powering
    /// the AI Settings slots' per-model ←/→ reasoning cycle.
    AiModelVariantsFetched(Result<std::collections::HashMap<String, Vec<String>>, String>),
    ShellIntegrationDetected(ShellIntegrationStatus),
}

/// Outcome of the post-attempt scan + commit task: either the fix AI made
/// no committable change (the row stays eligible), or the attempt was
/// committed by the harness.
enum BugkillCommitOutcome {
    NoChanges,
    Committed {
        sha: String,
        changes: AttemptChanges,
    },
}

struct MergePrDetailsPayload {
    title: String,
    body: String,
    /// Local commits on the worktree not yet pushed to its tracking remote.
    /// Drives the "push before merging?" guard on the confirm screen.
    unpushed_commits: u64,
}

struct MergePrFailure {
    number: u64,
    message: String,
}

/// Everything the background merge task needs: the `gh pr merge` subject +
/// body, plus the worktree to (optionally) `git push origin HEAD` from
/// before merging so unpushed local commits reach the PR first.
struct MergeExecution {
    number: u64,
    subject: String,
    body: String,
    worktree_path: String,
    push_first: bool,
}

struct UpdatePrSuccess {
    number: u64,
    base_ref: String,
    outcome: crate::services::UpdatePullRequestOutcome,
}

struct UpdatePrFailure {
    number: u64,
    message: String,
}

#[derive(Debug, Default)]
struct DashboardNotificationSnapshot {
    ai_statuses: HashMap<String, AiStatus>,
    pr_check_statuses: HashMap<u64, CheckStatus>,
}

impl DashboardNotificationSnapshot {
    fn record_update(&mut self, update: &DashboardUpdate) {
        self.ai_statuses = ai_statuses_by_worktree(update.rows());
        if let DashboardUpdate::WithPRs { rows, .. } = update {
            self.pr_check_statuses = pr_check_statuses_by_pr(rows);
        }
    }
}

fn dashboard_update_requests_bell(
    snapshot: &mut Option<DashboardNotificationSnapshot>,
    update: &DashboardUpdate,
    notifications: &NotificationsConfig,
) -> bool {
    let requests_bell = snapshot.as_ref().is_some_and(|previous| {
        (notifications.ai_status_ok && ai_finished_transition(previous, update.rows()))
            || (notifications.pr_checks_ok && pr_checks_passed_transition(previous, update))
    });

    snapshot
        .get_or_insert_with(Default::default)
        .record_update(update);
    requests_bell
}

fn ai_statuses_by_worktree(rows: &[DashboardRow]) -> HashMap<String, AiStatus> {
    rows.iter()
        .filter_map(|row| {
            row.ai_status
                .as_ref()
                .map(|report| (row.worktree.path.clone(), report.aggregated))
        })
        .collect()
}

fn pr_check_statuses_by_pr(rows: &[DashboardRow]) -> HashMap<u64, CheckStatus> {
    rows.iter()
        .filter_map(|row| {
            let pr = row.pull_request.as_ref()?;
            if pr.state != PrState::Open {
                return None;
            }
            pr.checks_status.map(|status| (pr.number, status))
        })
        .collect()
}

fn ai_finished_transition(previous: &DashboardNotificationSnapshot, rows: &[DashboardRow]) -> bool {
    rows.iter().any(|row| {
        let Some(next) = row.ai_status.as_ref().map(|report| report.aggregated) else {
            return false;
        };
        next == AiStatus::Finished
            && previous.ai_statuses.get(&row.worktree.path) == Some(&AiStatus::InProgress)
    })
}

fn pr_checks_passed_transition(
    previous: &DashboardNotificationSnapshot,
    update: &DashboardUpdate,
) -> bool {
    let DashboardUpdate::WithPRs { rows, .. } = update else {
        return false;
    };

    rows.iter().any(|row| {
        let Some(pr) = row.pull_request.as_ref() else {
            return false;
        };
        if pr.state != PrState::Open || pr.checks_status != Some(CheckStatus::Passed) {
            return false;
        }
        previous
            .pr_check_statuses
            .get(&pr.number)
            .is_some_and(|status| *status != CheckStatus::Passed)
    })
}

/// State the TUI carries across frames.
pub struct App {
    pub screen: Screen,
    pub is_from_wrapper: bool,
    phase: InitPhase,
    error: Option<String>,
    show_reset_confirm: bool,
    last_menu_index: usize,
    tick: usize,
    worktree_service: Option<WorktreeService>,
    git_root: Option<String>,
    quit_requested: bool,
    menu: Option<MenuScreen>,
    dashboard: Option<DashboardScreen>,
    dashboard_watch: Option<DashboardWatch>,
    dashboard_notification_snapshot: Option<DashboardNotificationSnapshot>,
    cache: Option<CacheScreen>,
    create: Option<CreateScreen>,
    delete: Option<DeleteScreen>,
    settings: Option<SettingsScreen>,
    setup: Option<SetupScreen>,
    setup_project: Option<SetupProjectScreen>,
    merge_pr: Option<MergePullRequestScreen>,
    update_pr: Option<UpdatePullRequestScreen>,
    explain_pr: Option<ExplainPullRequestScreen>,
    fix_pr: Option<FixPullRequestScreen>,
    review_pr: Option<ReviewPullRequestScreen>,
    bugkill_pr: Option<BugkillPullRequestScreen>,
    /// Watches opencode's database for the embedded investigation TUI's
    /// turn to complete — the TUI never exits on its own, so this is what
    /// advances the `Investigating` step. `Some` only while investigating.
    bugkill_investigation: Option<OpencodeTurnWatcher>,
    /// Same idea for the Explain screen's drafting TUI — advances straight
    /// to Review once opencode finishes writing `pull_request.md`. `Some`
    /// only while the `Explaining` step is active.
    explain_draft: Option<OpencodeTurnWatcher>,
    /// Same idea for the Fix screen's apply TUI in Autonomous mode — commits
    /// each fix + replies the moment opencode's turn finishes, so the user
    /// never has to press Enter. `Some` only while an autonomous `Applying`
    /// step is live.
    fix_apply_watch: Option<OpencodeTurnWatcher>,
    /// Same idea for Update PR's (and "Update branch (locally)"'s) conflict-
    /// resolution TUI — marks the AI done automatically once opencode
    /// finishes. `Some` only while that AI is actively streaming.
    update_conflict: Option<OpencodeTurnWatcher>,
    update_branch: Option<UpdateBranchScreen>,
    /// Fullscreen "Select AI provider/model" picker. Spawned as a modal on
    /// top of the Settings screen — when active the Settings state is
    /// preserved so the user lands back on the dashboard editor on exit.
    ai_model_picker: Option<AiModelPickerScreen>,
    shell_integration_status: Option<ShellIntegrationStatus>,
    toast: ToastState,
    last_rendered_buffer: Option<Buffer>,
    mouse_selection: Option<MouseSelection>,
    /// Wrapper-mode side channel: the path that should be emitted on real
    /// stdout once the TUI tears down. Only set in `is_from_wrapper` mode.
    selected_path: Option<String>,
    pending_delete_path: Option<String>,
    /// Worktree paths queued by a dashboard bulk-delete button; consumed
    /// once the Delete screen finishes loading.
    pending_bulk_delete_paths: Vec<String>,
    /// Remaining `(path, force)` items still to delete in the current
    /// bulk run, processed one at a time via `kick_off_delete_worktree`.
    bulk_delete_queue: Vec<(String, bool)>,
    /// In-flight "Update all" batch (Branches or Pull Requests). `None` when
    /// idle. Drives sequential per-worktree updates, auto-resolving conflicts
    /// via opencode and advancing without user interaction.
    update_all: Option<UpdateAllRun>,
    /// Whether an embedded opencode PTY was alive on the previous frame.
    /// A torn-down PTY can leave the primary-screen terminal scrolled out
    /// of sync with Ratatui's diff model, so we force one full repaint on
    /// the frame after the PTY disappears. See `event_loop_inner`.
    pty_was_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateAllKind {
    Branches,
    PullRequests,
}

/// State for an in-flight "Update all" batch: a queue of worktrees to update
/// one at a time, plus running tallies for the final summary toast. Conflict
/// resolution reuses the single-worktree opencode PTY machinery; the tick
/// driver (`on_update_all_tick`) auto-commits and advances the queue.
struct UpdateAllRun {
    kind: UpdateAllKind,
    /// Remaining `(worktree_path, branch)` targets for a Branches run.
    branch_queue: Vec<(String, String)>,
    /// Remaining PR update requests for a Pull Requests run.
    pr_queue: Vec<UpdatePullRequestRequest>,
    total: usize,
    /// Clean fetch/merge (or push) with no AI needed.
    updated: usize,
    /// Merge conflicts resolved by opencode and committed.
    resolved: usize,
    /// Already up to date / dirty tree / no base ref — nothing to do.
    skipped: usize,
    /// Per-worktree failure messages surfaced as warning toasts at the end.
    failed: Vec<String>,
}

impl UpdateAllRun {
    fn branches(targets: Vec<(String, String)>) -> Self {
        Self {
            kind: UpdateAllKind::Branches,
            total: targets.len(),
            branch_queue: targets,
            pr_queue: Vec::new(),
            updated: 0,
            resolved: 0,
            skipped: 0,
            failed: Vec::new(),
        }
    }

    fn pull_requests(targets: Vec<UpdatePullRequestRequest>) -> Self {
        Self {
            kind: UpdateAllKind::PullRequests,
            total: targets.len(),
            branch_queue: Vec::new(),
            pr_queue: targets,
            updated: 0,
            resolved: 0,
            skipped: 0,
            failed: Vec::new(),
        }
    }
}

impl App {
    pub fn new(initial_mode: AppMode, is_from_wrapper: bool) -> Self {
        Self {
            screen: Screen::from_mode(initial_mode),
            is_from_wrapper,
            phase: InitPhase::Loading,
            error: None,
            show_reset_confirm: false,
            last_menu_index: 0,
            tick: 0,
            worktree_service: None,
            git_root: None,
            quit_requested: false,
            menu: None,
            dashboard: None,
            dashboard_watch: None,
            dashboard_notification_snapshot: None,
            cache: None,
            create: None,
            delete: None,
            settings: None,
            setup: None,
            setup_project: None,
            merge_pr: None,
            update_pr: None,
            explain_pr: None,
            fix_pr: None,
            review_pr: None,
            bugkill_pr: None,
            bugkill_investigation: None,
            explain_draft: None,
            fix_apply_watch: None,
            update_conflict: None,
            update_branch: None,
            ai_model_picker: None,
            shell_integration_status: None,
            toast: ToastState::default(),
            last_rendered_buffer: None,
            mouse_selection: None,
            selected_path: None,
            pending_delete_path: None,
            pending_bulk_delete_paths: Vec::new(),
            bulk_delete_queue: Vec::new(),
            update_all: None,
            pty_was_active: false,
        }
    }

    /// In wrapper mode: the path the user picked, if any. `None` for any
    /// non-selection exit (Esc, Ctrl+C, error, cancel) — the wrapper's
    /// `[ -n "$dir" ]` check then short-circuits the `cd`.
    pub fn selected_path(&self) -> Option<&str> {
        self.selected_path.as_deref()
    }

    /// Drive the TUI: enter alt-screen, run the event loop until the user
    /// quits, then restore the terminal. Returns the selected path in
    /// wrapper mode (or `None` for any non-selection exit). In normal mode
    /// the return value is always `None` and ignored.
    pub async fn run(mut self) -> anyhow::Result<Option<String>> {
        terminal::install_panic_hook();
        if self.is_from_wrapper {
            let mut terminal = terminal::enter_wrapper().map_err(|e| {
                anyhow::anyhow!(
                    "wisetree --from-wrapper requires a controlling terminal \
                     (could not open the TTY: {e}). If you're invoking this \
                     manually, drop the --from-wrapper flag."
                )
            })?;
            let result = self.event_loop(&mut terminal).await;
            // Wrapper mode renders into a fixed bottom viewport on `/dev/tty`.
            // Clear the screen and reset the cursor so the shell prompt
            // returns at the top instead of below a block of empty rows.
            let _ = terminal::clear_wrapper_for_shell(&mut terminal);
            terminal::restore_wrapper_tty();
            let _ = terminal.show_cursor();
            result?;
        } else {
            let mut terminal = terminal::enter()?;
            let result = self.event_loop(&mut terminal).await;
            let _ = terminal.clear();
            terminal::restore();
            let _ = terminal.show_cursor();
            result?;
        }
        Ok(self.selected_path.clone())
    }

    async fn event_loop<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut ratatui::Terminal<B>,
    ) -> anyhow::Result<()> {
        let local = tokio::task::LocalSet::new();
        local.run_until(self.event_loop_inner(terminal)).await
    }

    async fn event_loop_inner<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut ratatui::Terminal<B>,
    ) -> anyhow::Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
        kick_off_initialize(tx.clone());

        let mut events = EventLoop::new(Duration::from_millis(50));
        let signal_quit = install_termination_listener();

        while !self.quit_requested && !signal_quit.load(Ordering::Relaxed) {
            while let Ok(event) = rx.try_recv() {
                self.handle_app_event(event, &tx);
            }
            self.poll_dashboard_updates();

            // An embedded opencode PTY (Fill / Update PR flows) drives the
            // child through a real terminal whose escape sequences can scroll
            // the primary screen out of sync with Ratatui's `Viewport::Fixed`
            // diff model. Once the PTY tears down, static regions Ratatui
            // thinks are unchanged (e.g. the header above the Fill "Done"
            // panel) never get repainted, so old scrollback bleeds through.
            // Force one full repaint on the frame after the PTY disappears.
            let pty_active = self.pty_active();
            if self.pty_was_active && !pty_active {
                terminal.clear()?;
            }
            self.pty_was_active = pty_active;

            let completed = terminal.draw(|frame| self.draw(frame))?;
            self.last_rendered_buffer = Some(completed.buffer.clone());

            match events.next_event()? {
                Event::Key(key) => self.handle_key(key, &tx),
                Event::Paste(text) => self.handle_paste(text, &tx),
                Event::Mouse(mouse) => self.handle_mouse(mouse, &tx),
                Event::Closed => self.quit_requested = true,
                Event::Tick => {
                    self.tick = self.tick.wrapping_add(1);
                    if let Some(screen) = self.update_pr.as_mut() {
                        // Resize tracking happens during render (where
                        // the panel area is known); the tick handles
                        // child-exit detection. `None` keeps the PTY at
                        // its last known size between resize events.
                        screen.tick_pty(None);
                    }
                    // The conflict-resolution TUI never exits on its own, so
                    // completion comes from the turn watcher polling
                    // opencode's database and marking the AI done
                    // automatically (same effect as the manual "Merge
                    // finalized?" confirm or a PTY exit, just without
                    // waiting on the user). Only poll while the AI is
                    // actively streaming; otherwise drop the watcher so a
                    // finished/absent screen doesn't linger.
                    let update_conflict_active = self
                        .update_pr
                        .as_ref()
                        .is_some_and(|s| s.ai_active() && !s.ai_done() && !s.terminal_active());
                    if update_conflict_active {
                        if let Some(turn) = self
                            .update_conflict
                            .as_mut()
                            .and_then(OpencodeTurnWatcher::poll)
                        {
                            self.on_update_conflict_turn(turn);
                        }
                    } else {
                        self.update_conflict = None;
                    }
                    // Drive the "Update all" batch: auto-commit once opencode
                    // finishes resolving conflicts, then advance the queue.
                    if self.update_all.is_some() {
                        self.on_update_all_tick(&tx);
                    }
                    // Same for the Explain PR PTY — but here a child exit means
                    // opencode finished drafting `pull_request.md`, so read
                    // the file and flip the screen into Review. The Explaining
                    // TUI also never exits on its own, so the turn watcher is
                    // the primary completion signal; PTY exit remains a
                    // fallback for a user who quits opencode manually.
                    let explain_status = self
                        .explain_pr
                        .as_mut()
                        .map(|screen| (screen.tick_pty(None), screen.is_explaining()));
                    match explain_status {
                        Some((true, _)) => {
                            self.explain_draft = None;
                            self.on_explain_ready_to_review(&tx);
                        }
                        Some((false, true)) => {
                            if let Some(turn) = self
                                .explain_draft
                                .as_mut()
                                .and_then(OpencodeTurnWatcher::poll)
                            {
                                self.on_explain_turn(turn, &tx);
                            }
                        }
                        Some((false, false)) | None => {
                            self.explain_draft = None;
                        }
                    }
                    // Same for the Fix PR apply PTY — a child exit means
                    // opencode finished editing, so commit + reply now (manual
                    // and autonomous alike). In Autonomous mode a finished
                    // opencode turn (detected via the database) also commits
                    // the fix, so wisetree advances without the user pressing
                    // Enter — exactly how Explain auto-advances on its turn.
                    let fix_status = self
                        .fix_pr
                        .as_mut()
                        .map(|screen| (screen.tick_pty(None), screen.step(), screen.autonomous()));
                    match fix_status {
                        Some((true, _, _)) => {
                            self.fix_apply_watch = None;
                            self.on_fix_apply_done(&tx);
                        }
                        Some((false, FixStep::Applying, true)) => {
                            if let Some(turn) = self
                                .fix_apply_watch
                                .as_mut()
                                .and_then(OpencodeTurnWatcher::poll)
                            {
                                self.on_fix_turn(turn, &tx);
                            }
                        }
                        _ => {
                            self.fix_apply_watch = None;
                        }
                    }
                    // Same for the Bugkill PTYs. The Investigating TUI never
                    // exits on its own, so completion comes from the turn
                    // watcher polling opencode's database; an early PTY exit
                    // there means the user quit opencode (or it crashed). A
                    // Fixing exit means the attempt is over, so scan +
                    // commit it now.
                    let bugkill_exited = self
                        .bugkill_pr
                        .as_mut()
                        .map(|screen| (screen.tick_pty(None), screen.step()));
                    match bugkill_exited {
                        Some((true, screens::bugkill_pr::BugkillStep::Investigating)) => {
                            self.on_bugkill_investigation_pty_exited(&tx)
                        }
                        Some((true, _)) => self.on_bugkill_fix_done(&tx),
                        Some((false, screens::bugkill_pr::BugkillStep::Investigating)) => {
                            if let Some(turn) = self
                                .bugkill_investigation
                                .as_mut()
                                .and_then(OpencodeTurnWatcher::poll)
                            {
                                self.on_bugkill_turn(turn, &tx);
                            }
                        }
                        _ => {}
                    }
                }
                Event::Resize(width, height) => {
                    // `Viewport::Fixed` (see `terminal::app_viewport`) does
                    // not auto-resize, so a terminal resize would leave the
                    // viewport at its original dimensions and corrupt every
                    // subsequent frame (clipped widgets, ghost cells from
                    // the previous size, mis-aligned borders). Explicitly
                    // resize the viewport to the new terminal size; ratatui
                    // also clears the screen as part of `resize`, so the
                    // next `terminal.draw` repaints cleanly.
                    terminal.resize(Rect::new(0, 0, width, height))?;
                    // Pixel coordinates of an in-progress text selection
                    // refer to the previous buffer dimensions; drop it so
                    // the user doesn't see ghost highlights at stale cells.
                    self.mouse_selection = None;
                }
            }
        }
        Ok(())
    }

    /// Whether any screen currently embeds a live opencode PTY. Used to
    /// detect the teardown edge that requires a full terminal repaint.
    fn pty_active(&self) -> bool {
        self.explain_pr.as_ref().is_some_and(|s| s.has_pty())
            || self.update_pr.as_ref().is_some_and(|s| s.has_pty())
            || self.bugkill_pr.as_ref().is_some_and(|s| s.has_pty())
            || self.fix_pr.as_ref().is_some_and(|s| s.has_pty())
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.toast.dismiss_expired();
        let area = frame.area();

        if area.width < 20 || area.height < 5 {
            let msg = Paragraph::new("Terminal too small").alignment(Alignment::Center);
            frame.render_widget(msg, area);
            return;
        }

        match self.phase {
            InitPhase::Loading => {
                // Render the WelcomeHeader on top of the loading splash so
                // the user (and integration tests) can immediately see which
                // screen is loading. Menu has its own header so we skip the
                // outer one in that case.
                if matches!(self.screen, Screen::Menu) {
                    screens::loading::draw(frame, area, self.tick, self.screen.as_str());
                } else {
                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(4), Constraint::Min(0)])
                        .split(area);
                    let cwd = self.git_root.as_deref().unwrap_or("");
                    WelcomeHeader::new(self.screen, cwd)
                        .with_label(self.header_label_override())
                        .render(frame, chunks[0]);
                    screens::loading::draw(frame, chunks[1], self.tick, self.screen.as_str());
                }
            }
            InitPhase::Errored => {
                let msg = self.error.as_deref().unwrap_or("Unknown error");
                screens::error::draw(frame, area, msg, self.show_reset_confirm);
            }
            InitPhase::Ready => self.draw_ready(frame, area),
        }

        if let (Some(snapshot), Some(selection)) = (
            self.last_rendered_buffer.as_ref(),
            self.mouse_selection.as_ref(),
        ) {
            frame.render_widget(SelectionOverlay::new(snapshot, selection), area);
        }

        if let Some(toast) = self.toast.current() {
            render_toast(frame, area, &toast);
        }
    }

    fn draw_ready(&mut self, frame: &mut Frame, area: Rect) {
        match self.screen {
            Screen::Menu => {
                if self.menu.is_none() {
                    self.menu = Some(self.build_menu_screen());
                }
                let menu = self.menu.as_mut().expect("menu set above");
                menu.render(frame, area);
            }
            Screen::Dashboard => {
                let panel = self.render_framed_panel_fill(frame, area);
                if let Some(dashboard) = self.dashboard.as_mut() {
                    dashboard.tick = self.tick;
                    dashboard.render(frame, panel);
                }
            }
            Screen::Cache => {
                let panel = self.render_framed_panel_fill(frame, area);
                if let Some(cache) = self.cache.as_mut() {
                    cache.tick = self.tick;
                    cache.render(frame, panel);
                }
            }
            Screen::Create => {
                let full = self.create.as_ref().is_some_and(|s| s.wants_full_height());
                let panel = if full {
                    self.render_framed_panel_fill(frame, area)
                } else {
                    let h = self
                        .create
                        .as_ref()
                        .map_or(8, |s| s.preferred_content_height());
                    self.render_framed_panel(frame, area, h)
                };
                if let Some(create) = self.create.as_mut() {
                    create.tick = self.tick;
                    create.render(frame, panel);
                }
            }
            Screen::Delete => {
                // When the single-target delete is awaiting confirmation,
                // render the dashboard underneath so the user sees the
                // worktree row they're about to remove. Bulk delete keeps
                // the dedicated `BulkConfirmDialog` layout (no overlay).
                let overlay_modal = self
                    .delete
                    .as_ref()
                    .filter(|d| matches!(d.step(), DeleteStep::Confirm))
                    .and_then(|d| d.overlay_modal().cloned());
                // While the worktree list is still loading for a single-path
                // delete (Backspace shortcut), the confirm modal isn't built
                // yet, so overlay_modal is None. Keep the dashboard visible
                // during that window to avoid a ~1 s blink before the modal
                // appears.
                let loading_single = self.pending_delete_path.is_some()
                    && self.delete.as_ref().map(|d| d.loading()).unwrap_or(false);
                if let Some(modal) = overlay_modal {
                    let panel = self.render_framed_panel_fill(frame, area);
                    if let Some(dashboard) = self.dashboard.as_mut() {
                        dashboard.tick = self.tick;
                        dashboard.render(frame, panel);
                    }
                    modal.render(frame, panel);
                } else if loading_single {
                    let panel = self.render_framed_panel_fill(frame, area);
                    if let Some(dashboard) = self.dashboard.as_mut() {
                        dashboard.tick = self.tick;
                        dashboard.render(frame, panel);
                    }
                } else {
                    let panel = match self.delete.as_ref().map(|s| s.step()) {
                        Some(DeleteStep::Confirm) => self.render_framed_panel_fill(frame, area),
                        _ => {
                            let h = self
                                .delete
                                .as_ref()
                                .map_or(8, |s| s.preferred_content_height());
                            self.render_framed_panel(frame, area, h)
                        }
                    };
                    if let Some(delete) = self.delete.as_mut() {
                        delete.tick = self.tick;
                        delete.render(frame, panel);
                    }
                }
            }
            Screen::Settings => {
                let panel = match self.settings.as_ref().map(|s| s.step()) {
                    Some(SettingsStep::Menu)
                    | Some(SettingsStep::DeleteBranch)
                    | Some(SettingsStep::AiSettings)
                    | None => self.render_framed_panel_fill(frame, area),
                    Some(_) => {
                        let h = self
                            .settings
                            .as_ref()
                            .map_or(14, |s| s.preferred_content_height());
                        self.render_framed_panel(frame, area, h)
                    }
                };
                if let Some(settings) = self.settings.as_mut() {
                    settings.tick = self.tick;
                    settings.render(frame, panel);
                }
            }
            Screen::Setup => {
                let panel = match self.setup.as_ref().map(|s| s.step()) {
                    Some(SetupStep::Confirm) => self.render_framed_panel_fill(frame, area),
                    _ => {
                        let h = self
                            .setup
                            .as_ref()
                            .map_or(8, |s| s.preferred_content_height());
                        self.render_framed_panel(frame, area, h)
                    }
                };
                if let Some(setup) = self.setup.as_mut() {
                    setup.tick = self.tick;
                    setup.render(frame, panel);
                }
            }
            Screen::MergePullRequest => {
                let panel = match self.merge_pr.as_ref().map(|s| s.step()) {
                    Some(MergeStep::Confirm) => self.render_framed_panel_fill(frame, area),
                    _ => {
                        let h = self
                            .merge_pr
                            .as_ref()
                            .map_or(8, |s| s.preferred_content_height());
                        self.render_framed_panel(frame, area, h)
                    }
                };
                if let Some(merge_pr) = self.merge_pr.as_mut() {
                    merge_pr.tick = self.tick;
                    merge_pr.render(frame, panel);
                }
            }
            Screen::SetupProject => {
                let panel = match self.setup_project.as_ref().map(|s| s.step()) {
                    // Preset list and confirm both benefit from the full panel:
                    // the list can show more options, and the confirm step keeps
                    // its Yes/No footer pinned below scrollable preset blocks.
                    Some(SetupProjectStep::PresetList) | Some(SetupProjectStep::Confirm) | None => {
                        self.render_framed_panel_fill(frame, area)
                    }
                    Some(SetupProjectStep::Discovering) => {
                        let h = self
                            .setup_project
                            .as_ref()
                            .map_or(12, |s| s.preferred_content_height());
                        self.render_framed_panel(frame, area, h)
                    }
                };
                if let Some(screen) = self.setup_project.as_mut() {
                    screen.tick = self.tick;
                    screen.render(frame, panel);
                }
            }
            Screen::UpdatePullRequest => {
                // Once the AI is actively streaming the conflict resolution —
                // or the push failed and the interactive Terminal Activity
                // shell is up — we want the entire bottom region of the
                // screen so these long-running, scroll-heavy panels have room
                // to breathe. The Confirm and pre-AI phases (Fetching,
                // Merging) stay in the compact framed panel so they don't look
                // lost in a huge empty area.
                let wants_fill = self.update_pr.as_ref().is_some_and(|s| {
                    (s.is_updating() && (s.ai_active() || s.terminal_active()))
                        || s.commit_push_running()
                });
                let in_confirm = self
                    .update_pr
                    .as_ref()
                    .is_some_and(|s| matches!(s.step(), UpdateStep::Confirm));
                let panel = if wants_fill || in_confirm {
                    self.render_framed_panel_fill(frame, area)
                } else {
                    let h = self
                        .update_pr
                        .as_ref()
                        .map_or(8, |s| s.preferred_content_height());
                    self.render_framed_panel(frame, area, h)
                };
                if let Some(update_pr) = self.update_pr.as_mut() {
                    update_pr.tick = self.tick;
                    update_pr.render(frame, panel);
                }
            }
            Screen::ExplainPullRequest => {
                // The Explaining step (live opencode PTY), the Confirm
                // explanation, and Opening's live Terminal Activity all want
                // the full bottom region. Loading / Review stay compact.
                let expand = self.explain_pr.as_ref().is_some_and(|s| {
                    s.is_explaining()
                        || matches!(s.step(), ExplainStep::Confirm | ExplainStep::Opening)
                });
                let panel = if expand {
                    self.render_framed_panel_fill(frame, area)
                } else {
                    let h = self
                        .explain_pr
                        .as_ref()
                        .map_or(8, |s| s.preferred_content_height());
                    self.render_framed_panel(frame, area, h)
                };
                if let Some(explain_pr) = self.explain_pr.as_mut() {
                    explain_pr.tick = self.tick;
                    explain_pr.render(frame, panel);
                }
            }
            Screen::FixPullRequest => {
                // Full-panel steps (the live apply PTY, the decision view, the
                // confirm explanation, the "Other" box) want the whole bottom
                // region; the compact Working / Done steps stay sized.
                let expand = self.fix_pr.as_ref().is_some_and(|s| s.wants_full_panel());
                let panel = if expand {
                    self.render_framed_panel_fill(frame, area)
                } else {
                    // The framed panel trims rounded borders (2) + horizontal
                    // padding (4) off the area; pass that inner width so the
                    // Working step can size its wrapped reviewer-comment panel.
                    let content_width = area.width.saturating_sub(6);
                    let h = self
                        .fix_pr
                        .as_ref()
                        .map_or(8, |s| s.preferred_content_height(content_width));
                    self.render_framed_panel(frame, area, h)
                };
                if let Some(fix_pr) = self.fix_pr.as_mut() {
                    fix_pr.tick = self.tick;
                    fix_pr.render(frame, panel);
                }
            }
            Screen::ReviewPullRequest => {
                // Full-panel steps (Confirm, Decision, Other, Summary) want
                // the whole bottom region; the compact Working / Done steps
                // stay sized.
                let expand = self
                    .review_pr
                    .as_ref()
                    .is_some_and(|s| s.wants_full_panel());
                let panel = if expand {
                    self.render_framed_panel_fill(frame, area)
                } else {
                    let h = self
                        .review_pr
                        .as_ref()
                        .map_or(8, |s| s.preferred_content_height());
                    self.render_framed_panel(frame, area, h)
                };
                if let Some(review_pr) = self.review_pr.as_mut() {
                    review_pr.tick = self.tick;
                    review_pr.render(frame, panel);
                }
            }
            Screen::BugkillPullRequest => {
                // Expanded steps (Confirm, DescribeBug, Select, the live
                // Fixing PTY, Verdict, Done…) want the whole bottom region;
                // the compact Working / ResumePrompt steps stay sized.
                let expand = self
                    .bugkill_pr
                    .as_ref()
                    .is_some_and(|s| s.wants_full_panel());
                let panel = if expand {
                    self.render_framed_panel_fill(frame, area)
                } else {
                    let h = self
                        .bugkill_pr
                        .as_ref()
                        .map_or(8, |s| s.preferred_content_height());
                    self.render_framed_panel(frame, area, h)
                };
                if let Some(bugkill_pr) = self.bugkill_pr.as_mut() {
                    bugkill_pr.tick = self.tick;
                    bugkill_pr.render(frame, panel);
                }
            }
            Screen::UpdateBranch => {
                let h = self
                    .update_branch
                    .as_ref()
                    .map_or(3, |s| s.preferred_content_height());
                let panel = self.render_framed_panel(frame, area, h);
                if let Some(update_branch) = self.update_branch.as_mut() {
                    update_branch.tick = self.tick;
                    update_branch.render(frame, panel);
                }
            }
            Screen::AiModelPicker => {
                let panel = self.render_framed_panel_fill(frame, area);
                if let Some(picker) = self.ai_model_picker.as_mut() {
                    picker.tick = self.tick;
                    picker.render(frame, panel);
                }
            }
        }
    }

    /// Header label override for screens reused across flows. The
    /// `UpdatePullRequest` screen also hosts the "Update branch (locally)"
    /// conflict resolution (`local_only`), which should read "Update Branch"
    /// rather than "Update Pull Request".
    fn header_label_override(&self) -> Option<&'static str> {
        if matches!(self.screen, Screen::UpdatePullRequest)
            && self.update_pr.as_ref().is_some_and(|s| s.local_only())
        {
            Some("Update Branch")
        } else {
            None
        }
    }

    fn render_framed_panel(&self, frame: &mut Frame, area: Rect, content_height: u16) -> Rect {
        let panel_height = content_height.saturating_add(2);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Length(panel_height),
                Constraint::Min(0),
            ])
            .split(area);

        let cwd = self.git_root.as_deref().unwrap_or("");
        WelcomeHeader::new(self.screen, cwd)
            .with_label(self.header_label_override())
            .render(frame, chunks[0]);

        self.render_panel_block(frame, chunks[1])
    }

    fn render_framed_panel_fill(&self, frame: &mut Frame, area: Rect) -> Rect {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(4), Constraint::Min(0)])
            .split(area);

        let cwd = self.git_root.as_deref().unwrap_or("");
        WelcomeHeader::new(self.screen, cwd)
            .with_label(self.header_label_override())
            .render(frame, chunks[0]);

        self.render_panel_block(frame, chunks[1])
    }

    fn render_panel_block(&self, frame: &mut Frame, area: Rect) -> Rect {
        let panel = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::MENU_BORDER).bg(colors::MENU_BG))
            .style(Style::default().bg(colors::MENU_BG));
        let inner = panel.inner(area);
        frame.render_widget(panel, area);
        Rect {
            x: inner.x.saturating_add(2),
            y: inner.y,
            width: inner.width.saturating_sub(4),
            height: inner.height,
        }
    }

    fn handle_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.quit_requested = true;
            return;
        }

        self.mouse_selection = None;

        match self.phase {
            InitPhase::Errored => self.handle_error_key(key, tx),
            InitPhase::Ready => self.handle_screen_key(key, tx),
            InitPhase::Loading => {}
        }
    }

    /// Route a bracketed-paste payload to the focused surface. The Bugkill
    /// screen consumes the whole payload atomically (multi-line bug reports);
    /// every other screen replays the text as plain key presses so the focused
    /// text input receives it, reusing the normal key dispatch without extra
    /// plumbing. Control characters (newlines, tabs, escapes) are dropped so a
    /// pasted trailing newline can't submit or cancel a prompt mid-paste.
    fn handle_paste(&mut self, text: String, tx: &mpsc::UnboundedSender<AppEvent>) {
        self.mouse_selection = None;
        if !matches!(self.phase, InitPhase::Ready) {
            return;
        }
        if matches!(self.screen, Screen::BugkillPullRequest) {
            let action = self
                .bugkill_pr
                .as_mut()
                .map(|screen| screen.handle_paste(&text))
                .unwrap_or(BugkillAction::Continue);
            self.apply_bugkill_action(action, tx);
            return;
        }
        for ch in text.chars() {
            if ch.is_control() {
                continue;
            }
            self.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), tx);
        }
    }

    fn scroll_screen(&mut self, direction: ScrollDirection, lines: u16) {
        match self.screen {
            Screen::UpdatePullRequest => {
                if let Some(screen) = self.update_pr.as_mut() {
                    match direction {
                        ScrollDirection::Up => screen.handle_mouse_scroll_up(lines),
                        ScrollDirection::Down => screen.handle_mouse_scroll_down(lines),
                    };
                }
            }
            Screen::Create => {
                if let Some(screen) = self.create.as_mut() {
                    match direction {
                        ScrollDirection::Up => screen.scroll_terminal_up(lines),
                        ScrollDirection::Down => screen.scroll_terminal_down(lines),
                    };
                }
            }
            Screen::ExplainPullRequest => {
                if let Some(screen) = self.explain_pr.as_mut() {
                    match direction {
                        ScrollDirection::Up => screen.handle_mouse_scroll_up(lines),
                        ScrollDirection::Down => screen.handle_mouse_scroll_down(lines),
                    };
                }
            }
            Screen::FixPullRequest => {
                if let Some(screen) = self.fix_pr.as_mut() {
                    match direction {
                        ScrollDirection::Up => screen.handle_mouse_scroll_up(lines),
                        ScrollDirection::Down => screen.handle_mouse_scroll_down(lines),
                    };
                }
            }
            Screen::ReviewPullRequest => {
                if let Some(screen) = self.review_pr.as_mut() {
                    match direction {
                        ScrollDirection::Up => screen.handle_mouse_scroll_up(lines),
                        ScrollDirection::Down => screen.handle_mouse_scroll_down(lines),
                    };
                }
            }
            Screen::BugkillPullRequest => {
                if let Some(screen) = self.bugkill_pr.as_mut() {
                    match direction {
                        ScrollDirection::Up => screen.handle_mouse_scroll_up(lines),
                        ScrollDirection::Down => screen.handle_mouse_scroll_down(lines),
                    };
                }
            }
            _ => {}
        }
    }

    /// Hand a mouse event to the active screen's focused inner PTY, if any.
    /// Returns true when the child (opencode) is tracking the mouse and took
    /// the event — the four PR-command screens that embed an opencode PTY each
    /// forward only while their inner panel holds focus.
    fn forward_mouse_to_focused_pty(&mut self, mouse: MouseEvent) -> bool {
        match self.screen {
            Screen::UpdatePullRequest => self
                .update_pr
                .as_mut()
                .is_some_and(|screen| screen.forward_pty_mouse(mouse)),
            Screen::ExplainPullRequest => self
                .explain_pr
                .as_mut()
                .is_some_and(|screen| screen.forward_pty_mouse(mouse)),
            Screen::FixPullRequest => self
                .fix_pr
                .as_mut()
                .is_some_and(|screen| screen.forward_pty_mouse(mouse)),
            Screen::BugkillPullRequest => self
                .bugkill_pr
                .as_mut()
                .is_some_and(|screen| screen.forward_pty_mouse(mouse)),
            _ => false,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        // When an embedded opencode PTY is focused, the mouse belongs to it:
        // opencode draws its own cursor and hover state from the reports we
        // forward, exactly as it would running standalone. If the child
        // consumed the event, skip the host's own selection / scroll handling.
        if self.forward_mouse_to_focused_pty(mouse) {
            return;
        }
        let Some(snapshot) = self.last_rendered_buffer.as_ref() else {
            return;
        };

        let raw_position = ratatui::layout::Position {
            x: mouse.column,
            y: mouse.row,
        };
        let clamped = clamp_position(raw_position, snapshot.area);

        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) && matches!(self.phase, InitPhase::Ready)
            && matches!(self.screen, Screen::SetupProject)
            && self
                .setup_project
                .as_mut()
                .is_some_and(|screen| screen.handle_mouse(mouse))
        {
            return;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.mouse_selection = contains_position(snapshot.area, raw_position)
                    .then(|| MouseSelection::start(raw_position));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let (Some(selection), Some(position)) = (self.mouse_selection.as_mut(), clamped)
                {
                    selection.update(position);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(mut selection) = self.mouse_selection.take() else {
                    return;
                };
                if let Some(position) = clamped {
                    selection.update(position);
                }

                // A click without drag is a button activation, not a text
                // selection. Try the dashboard's bulk-delete buttons first;
                // fall back to clipboard copy when the click missed.
                if let Some(text) = extract_text(snapshot, &selection) {
                    kick_off_clipboard_copy(text, "Copied to clipboard".to_string(), tx.clone());
                    return;
                }
                self.handle_screen_mouse_click(raw_position, tx);
            }
            MouseEventKind::ScrollUp => {
                // Web-page semantics: wheel scrolls the screen's active
                // scrollable region. On Update Pull Request that is either
                // the live AI Activity panel (during conflict resolution) or
                // the review diff panel (after the AI creates a merge commit);
                // on Create it's the "Creating" Terminal Activity log.
                self.scroll_screen(ScrollDirection::Up, WHEEL_LINES_PER_TICK);
            }
            MouseEventKind::ScrollDown => {
                self.scroll_screen(ScrollDirection::Down, WHEEL_LINES_PER_TICK);
            }
            _ => {}
        }
    }

    fn handle_error_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        if self.show_reset_confirm {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.show_reset_confirm = false;
                    match reset_global_config() {
                        Ok(()) => {
                            self.error = None;
                            self.phase = InitPhase::Loading;
                            kick_off_initialize(tx.clone());
                        }
                        Err(e) => {
                            self.error = Some(format!("Failed to reset configuration: {e}"));
                        }
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.show_reset_confirm = false;
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.show_reset_confirm = true;
            }
            _ => {
                self.error = None;
                self.phase = InitPhase::Ready;
                self.back_to_menu();
            }
        }
    }

    fn handle_screen_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        match self.screen {
            Screen::Menu => self.handle_menu_key(key, tx),
            Screen::Dashboard => self.handle_dashboard_key(key, tx),
            Screen::Cache => self.handle_cache_key(key, tx),
            Screen::Create => self.handle_create_key(key, tx),
            Screen::Delete => self.handle_delete_key(key, tx),
            Screen::Settings => self.handle_settings_key(key, tx),
            Screen::Setup => self.handle_setup_key(key, tx),
            Screen::SetupProject => self.handle_setup_project_key(key, tx),
            Screen::MergePullRequest => self.handle_merge_pr_key(key, tx),
            Screen::UpdatePullRequest => self.handle_update_pr_key(key, tx),
            Screen::ExplainPullRequest => self.handle_explain_pr_key(key, tx),
            Screen::FixPullRequest => self.handle_fix_pr_key(key, tx),
            Screen::ReviewPullRequest => self.handle_review_pr_key(key, tx),
            Screen::BugkillPullRequest => self.handle_bugkill_key(key, tx),
            Screen::UpdateBranch => {
                if let Some(screen) = self.update_branch.as_mut() {
                    screen.handle_key(key);
                }
            }
            Screen::AiModelPicker => self.handle_ai_model_picker_key(key, tx),
        }
    }

    fn handle_screen_mouse_click(
        &mut self,
        position: Position,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        match self.screen {
            Screen::Menu => {
                if self.menu.is_none() {
                    self.menu = Some(self.build_menu_screen());
                }
                let Some(menu) = self.menu.as_mut() else {
                    return;
                };
                match menu.handle_mouse_click(position) {
                    MenuOutcome::Selected(choice, idx) => {
                        self.last_menu_index = idx;
                        match choice {
                            MenuChoice::Exit => self.quit_requested = true,
                            MenuChoice::Setup => self.enter_screen(Screen::Setup, tx),
                            MenuChoice::Create => self.enter_screen(Screen::Create, tx),
                            MenuChoice::Dashboard => self.enter_screen(Screen::Dashboard, tx),
                            MenuChoice::Cache => self.enter_screen(Screen::Cache, tx),
                            MenuChoice::Settings => self.enter_screen(Screen::Settings, tx),
                        }
                    }
                    MenuOutcome::Cancelled => self.quit_requested = true,
                    MenuOutcome::Pending => {}
                }
            }
            Screen::Dashboard => {
                let action = self
                    .dashboard
                    .as_mut()
                    .map(|dashboard| dashboard.handle_mouse_click(position))
                    .unwrap_or(DashboardAction::Continue);
                self.apply_dashboard_action(action, tx);
            }
            Screen::Cache => {
                let action = self
                    .cache
                    .as_mut()
                    .map(|cache| cache.handle_mouse_click(position))
                    .unwrap_or(CacheScreenAction::Continue);
                match action {
                    CacheScreenAction::Continue => {}
                    CacheScreenAction::Back => self.back_to_menu(),
                    CacheScreenAction::Refresh => {
                        if let Some(cache) = self.cache.as_mut() {
                            cache.start_loading();
                        }
                        kick_off_cache_load(self.git_root.clone(), tx.clone());
                    }
                    CacheScreenAction::DeleteEntry(relative_path) => {
                        if let Some(cache) = self.cache.as_mut() {
                            cache.start_loading();
                        }
                        kick_off_cache_entry_delete(
                            self.git_root.clone(),
                            relative_path,
                            tx.clone(),
                        );
                    }
                }
            }
            Screen::Create => {
                let action = self
                    .create
                    .as_mut()
                    .map(|create| create.handle_mouse_click(position))
                    .unwrap_or(CreateAction::Continue);
                match action {
                    CreateAction::Continue => {}
                    CreateAction::Cancelled => self.back_to_menu(),
                    CreateAction::Confirmed {
                        directory_name,
                        source_branch,
                        new_branch,
                    } => {
                        if let Some(create) = self.create.as_mut() {
                            create.start_creating();
                        }

                        let options = WorktreeCreateOptions {
                            name: directory_name,
                            source_branch,
                            new_branch,
                            base_path: self.git_root.clone().unwrap_or_default(),
                        };
                        kick_off_create_worktree(self.git_root.clone(), options, tx.clone());
                    }
                    CreateAction::Done => self.finish_create_success(),
                }
            }
            Screen::Delete => {
                let action = self
                    .delete
                    .as_mut()
                    .map(|delete| delete.handle_mouse_click(position))
                    .unwrap_or(DeleteAction::Continue);
                match action {
                    DeleteAction::Continue => {}
                    DeleteAction::Cancelled => {
                        self.cancel_delete_screen(tx);
                    }
                    DeleteAction::Confirmed { path, force } => {
                        if let Some(delete) = self.delete.as_mut() {
                            delete.start_deleting();
                        }
                        kick_off_delete_worktree(self.git_root.clone(), path, force, tx.clone());
                    }
                    DeleteAction::BulkConfirmed { items } => {
                        self.bulk_delete_queue = items;
                        if let Some(delete) = self.delete.as_mut() {
                            delete.start_deleting();
                        }
                        self.dispatch_next_bulk_delete(tx);
                    }
                    DeleteAction::Done => self.leave_delete_screen(tx),
                }
            }
            Screen::Settings => {
                let action = self
                    .settings
                    .as_mut()
                    .map(|settings| settings.handle_mouse_click(position))
                    .unwrap_or(SettingsAction::Continue);
                match action {
                    SettingsAction::Continue => {}
                    SettingsAction::Back => self.back_to_menu(),
                    SettingsAction::CopySettingsFilePath => {
                        let path = self.settings_edit_file_path().display().to_string();
                        kick_off_clipboard_copy(
                            path,
                            SETTINGS_PATH_COPIED_MESSAGE.to_string(),
                            tx.clone(),
                        );
                    }
                    SettingsAction::CheckUpdates => {
                        if let Some(settings) = self.settings.as_mut() {
                            settings.start_checking_updates();
                        }
                        kick_off_update_check(tx.clone());
                    }
                    SettingsAction::SetDeleteBranchWithWorktree(enabled) => {
                        if let Err(err) = self.save_delete_branch_with_worktree(enabled) {
                            if let Some(settings) = self.settings.as_mut() {
                                settings
                                    .set_error(format!("Failed to update configuration: {err}"));
                            }
                        }
                    }
                    SettingsAction::Reset => {
                        if let Err(err) = self.reset_settings_config() {
                            if let Some(settings) = self.settings.as_mut() {
                                settings.set_error(format!("Failed to reset configuration: {err}"));
                            }
                        }
                    }
                    SettingsAction::SaveCopyPatterns(patterns) => {
                        if let Err(err) = self.save_copy_patterns(patterns) {
                            if let Some(settings) = self.settings.as_mut() {
                                settings.set_error(format!("Failed to save copy patterns: {err}"));
                            }
                        }
                    }
                    SettingsAction::SaveIgnorePatterns(patterns) => {
                        if let Err(err) = self.save_ignore_patterns(patterns) {
                            if let Some(settings) = self.settings.as_mut() {
                                settings
                                    .set_error(format!("Failed to save ignore patterns: {err}"));
                            }
                        }
                    }
                    SettingsAction::SaveLinkPatterns(patterns) => {
                        if let Err(err) = self.save_link_patterns(patterns) {
                            if let Some(settings) = self.settings.as_mut() {
                                settings.set_error(format!("Failed to save link patterns: {err}"));
                            }
                        }
                    }
                    SettingsAction::CopySettings(direction) => {
                        if let Err(err) = self.copy_settings(direction) {
                            if let Some(settings) = self.settings.as_mut() {
                                settings.set_error(format!("Failed to copy settings: {err}"));
                            }
                        }
                    }
                    SettingsAction::SavePostCreateCommands(commands) => {
                        if let Err(err) = self.save_post_create_commands(commands) {
                            if let Some(settings) = self.settings.as_mut() {
                                settings.set_error(format!(
                                    "Failed to save post-create commands: {err}"
                                ));
                            }
                        }
                    }
                    SettingsAction::SaveTerminalCommand(command) => {
                        if let Err(err) = self.save_terminal_command(command) {
                            if let Some(settings) = self.settings.as_mut() {
                                settings
                                    .set_error(format!("Failed to save terminal command: {err}"));
                            }
                        }
                    }
                    SettingsAction::SavePathTemplate(template) => {
                        if let Err(err) = self.save_path_template(template) {
                            if let Some(settings) = self.settings.as_mut() {
                                settings.set_error(format!("Failed to save path template: {err}"));
                            }
                        }
                    }
                    SettingsAction::SaveLinkStrategy(strategy) => {
                        if let Err(err) = self.save_link_strategy(strategy) {
                            if let Some(settings) = self.settings.as_mut() {
                                settings.set_error(format!("Failed to save link strategy: {err}"));
                            }
                        }
                    }
                    SettingsAction::SaveLinkCacheDir(cache_dir) => {
                        if let Err(err) = self.save_link_cache_dir(cache_dir) {
                            if let Some(settings) = self.settings.as_mut() {
                                settings.set_error(format!("Failed to save link cache dir: {err}"));
                            }
                        }
                    }
                    SettingsAction::SaveDashboard(dashboard) => {
                        if let Err(err) = self.save_dashboard(*dashboard) {
                            if let Some(settings) = self.settings.as_mut() {
                                settings
                                    .set_error(format!("Failed to save dashboard settings: {err}"));
                            }
                        }
                    }
                    SettingsAction::SaveNotifications(notifications) => {
                        if let Err(err) = self.save_notifications(notifications) {
                            if let Some(settings) = self.settings.as_mut() {
                                settings.set_error(format!(
                                    "Failed to save notification settings: {err}"
                                ));
                            }
                        }
                    }
                    SettingsAction::OpenAiModelPicker { model, variant } => {
                        self.open_ai_model_picker(model, variant, tx);
                    }
                    SettingsAction::FetchFreeModels => {
                        kick_off_fetch_free_opencode_models(tx.clone());
                        kick_off_fetch_ai_model_variants(tx.clone());
                    }
                    SettingsAction::UpgradeSource(source) => {
                        if let Some(settings) = self.settings.as_mut() {
                            settings.start_upgrade(source);
                        }
                        kick_off_upgrade(source, tx.clone());
                    }
                    SettingsAction::OpenSetupProject => {
                        self.enter_screen(Screen::SetupProject, tx);
                    }
                }
            }
            Screen::Setup => {
                let action = self
                    .setup
                    .as_mut()
                    .map(|setup| setup.handle_mouse_click(position))
                    .unwrap_or(SetupAction::Continue);
                match action {
                    SetupAction::Continue => {}
                    SetupAction::Cancelled => self.back_to_menu(),
                    SetupAction::Confirmed { shell } => {
                        if let Some(setup) = self.setup.as_mut() {
                            setup.start_installing();
                        }
                        kick_off_setup_install(shell, tx.clone());
                    }
                    SetupAction::Done => self.back_to_menu(),
                }
            }
            Screen::SetupProject => {
                let action = self
                    .setup_project
                    .as_mut()
                    .map(|screen| screen.handle_mouse_click(position))
                    .unwrap_or(SetupProjectAction::Continue);
                match action {
                    SetupProjectAction::Continue => {}
                    SetupProjectAction::Cancelled => self.back_to_menu(),
                    SetupProjectAction::DiscoverWise => self.start_wise_preset_discovery(tx),
                    SetupProjectAction::Apply(preset) => self.apply_setup_project_preset(preset),
                }
            }
            Screen::MergePullRequest => {
                let action = self
                    .merge_pr
                    .as_mut()
                    .map(|screen| screen.handle_mouse_click(position))
                    .unwrap_or(MergeAction::Continue);
                match action {
                    MergeAction::Continue => {}
                    MergeAction::Cancelled => {
                        let worktree_path = self
                            .merge_pr
                            .take()
                            .map(|s| s.request().worktree_path.clone());
                        self.back_to_dashboard_action_menu(worktree_path, tx);
                    }
                    MergeAction::Confirmed {
                        number,
                        title,
                        body,
                        worktree_path,
                        push_first,
                    } => {
                        if let Some(screen) = self.merge_pr.as_mut() {
                            screen.start_merging(push_first);
                        }
                        kick_off_merge_pull_request(
                            self.git_root.clone(),
                            self.current_dashboard_config(),
                            MergeExecution {
                                number,
                                subject: title,
                                body,
                                worktree_path,
                                push_first,
                            },
                            tx.clone(),
                        );
                    }
                }
            }
            Screen::UpdatePullRequest => {
                let action = self
                    .update_pr
                    .as_mut()
                    .map(|screen| screen.handle_mouse_click(position))
                    .unwrap_or(UpdateAction::Continue);
                match action {
                    UpdateAction::Continue => {}
                    UpdateAction::Cancelled => {
                        let worktree_path = self
                            .update_pr
                            .as_ref()
                            .map(|s| s.request().worktree_path.clone());
                        if let Some(screen) = self.update_pr.as_ref() {
                            if screen.ai_active() {
                                let request = screen.request().clone();
                                let git_root = self.git_root.clone();
                                let dashboard_config = self.current_dashboard_config();
                                kick_off_abort_ai_merge(
                                    git_root,
                                    dashboard_config,
                                    request,
                                    tx.clone(),
                                );
                            }
                        }
                        self.update_pr = None;
                        self.back_to_dashboard_action_menu(worktree_path, tx);
                    }
                    UpdateAction::Confirmed => self.confirm_update_pr(tx),
                    UpdateAction::AiComplete => self.start_commit_after_ai(),
                    UpdateAction::AiCancel => {
                        let dashboard_config = self.current_dashboard_config();
                        let git_root = self.git_root.clone();
                        let Some(screen) = self.update_pr.as_mut() else {
                            return;
                        };
                        let request = screen.request().clone();
                        screen.set_phase_message("Aborting merge and discarding AI changes...");
                        kick_off_abort_ai_merge(git_root, dashboard_config, request, tx.clone());
                    }
                    UpdateAction::TerminalAccept => self.terminal_accept_push(tx),
                    UpdateAction::TerminalDiscard => self.terminal_discard(tx),
                }
            }
            Screen::ExplainPullRequest => {
                let action = self
                    .explain_pr
                    .as_mut()
                    .map(|screen| screen.handle_mouse_click(position))
                    .unwrap_or(ExplainAction::Continue);
                self.apply_explain_action(action, tx);
            }
            Screen::FixPullRequest => {
                let action = self
                    .fix_pr
                    .as_mut()
                    .map(|screen| screen.handle_mouse_click(position))
                    .unwrap_or(FixAction::Continue);
                self.apply_fix_action(action, tx);
            }
            Screen::ReviewPullRequest => {
                let action = self
                    .review_pr
                    .as_mut()
                    .map(|screen| screen.handle_mouse_click(position))
                    .unwrap_or(ReviewAction::Continue);
                self.apply_review_action(action, tx);
            }
            Screen::BugkillPullRequest => {
                let action = self
                    .bugkill_pr
                    .as_mut()
                    .map(|screen| screen.handle_mouse_click(position))
                    .unwrap_or(BugkillAction::Continue);
                self.apply_bugkill_action(action, tx);
            }
            Screen::UpdateBranch => {}
            Screen::AiModelPicker => {
                let action = match self.ai_model_picker.as_mut() {
                    Some(picker) => picker.handle_mouse_click(position),
                    None => {
                        self.close_ai_model_picker();
                        return;
                    }
                };

                match action {
                    AiModelPickerAction::Continue => {}
                    AiModelPickerAction::Cancelled => self.close_ai_model_picker(),
                    AiModelPickerAction::Selected { model, variant } => {
                        if let Some(settings) = self.settings.as_mut() {
                            settings.apply_ai_selection(model, variant);
                        }
                        self.close_ai_model_picker();
                    }
                }
            }
        }
    }

    fn handle_ai_model_picker_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.ai_model_picker.as_mut() {
            Some(picker) => picker.handle_key(key),
            None => {
                self.close_ai_model_picker();
                return;
            }
        };

        match action {
            AiModelPickerAction::Continue => {}
            AiModelPickerAction::Cancelled => self.close_ai_model_picker(),
            AiModelPickerAction::Selected { model, variant } => {
                // Stamp the chosen model + thinking strength into the still-live
                // Dashboard editor and drop back onto it — the user persists the
                // change by pressing the editor's Save button (same pattern as
                // every other dashboard field). Auto-saving here would route the
                // user past the editor to the Settings menu, which they
                // don't expect.
                if let Some(settings) = self.settings.as_mut() {
                    settings.apply_ai_selection(model, variant);
                }
                self.close_ai_model_picker();
                let _ = tx;
            }
        }
    }

    /// Push the picker on top of the still-alive Settings screen, kick off the
    /// background catalogue fetch, and flip the route. The picker reads the
    /// current `model` / `variant` so reopening it lands on the user's prior
    /// choice (and pre-selects their thinking strength).
    fn open_ai_model_picker(
        &mut self,
        model: String,
        variant: String,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        self.ai_model_picker = Some(AiModelPickerScreen::new(model, variant));
        self.screen = Screen::AiModelPicker;
        kick_off_fetch_opencode_models(tx.clone());
    }

    /// Tear down the picker overlay and return to the underlying Settings
    /// screen. `clear_screen_state` is deliberately *not* called — the
    /// Settings instance must survive so the dashboard editor remains visible.
    fn close_ai_model_picker(&mut self) {
        self.ai_model_picker = None;
        self.screen = Screen::Settings;
    }

    fn handle_merge_pr_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.merge_pr.as_mut() {
            Some(screen) => screen.handle_key(key),
            None => return,
        };
        match action {
            MergeAction::Continue => {}
            MergeAction::Cancelled => {
                let worktree_path = self
                    .merge_pr
                    .take()
                    .map(|s| s.request().worktree_path.clone());
                self.back_to_dashboard_action_menu(worktree_path, tx);
            }
            MergeAction::Confirmed {
                number,
                title,
                body,
                worktree_path,
                push_first,
            } => {
                if let Some(screen) = self.merge_pr.as_mut() {
                    screen.start_merging(push_first);
                }
                kick_off_merge_pull_request(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    MergeExecution {
                        number,
                        subject: title,
                        body,
                        worktree_path,
                        push_first,
                    },
                    tx.clone(),
                );
            }
        }
    }

    fn handle_update_pr_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.update_pr.as_mut() {
            Some(screen) => screen.handle_key(key),
            None => return,
        };
        match action {
            UpdateAction::Continue => {}
            UpdateAction::Cancelled => {
                // If the merge was already in flight (conflicts detected and
                // AI handed control back to the user), leaving without
                // `git merge --abort` strands the worktree with conflict
                // markers and a half-applied merge. Run the same cleanup
                // path as AiCancel before we navigate away.
                let worktree_path = self
                    .update_pr
                    .as_ref()
                    .map(|s| s.request().worktree_path.clone());
                if let Some(screen) = self.update_pr.as_ref() {
                    if screen.ai_active() {
                        let request = screen.request().clone();
                        let git_root = self.git_root.clone();
                        let dashboard_config = self.current_dashboard_config();
                        kick_off_abort_ai_merge(git_root, dashboard_config, request, tx.clone());
                    }
                }
                self.update_pr = None;
                self.back_to_dashboard_action_menu(worktree_path, tx);
            }
            UpdateAction::Confirmed => self.confirm_update_pr(tx),
            UpdateAction::AiComplete => self.start_commit_after_ai(),
            UpdateAction::AiCancel => {
                let dashboard_config = self.current_dashboard_config();
                let git_root = self.git_root.clone();
                let Some(screen) = self.update_pr.as_mut() else {
                    return;
                };
                let request = screen.request().clone();
                screen.set_phase_message("Aborting merge and discarding AI changes...");
                kick_off_abort_ai_merge(git_root, dashboard_config, request, tx.clone());
            }
            UpdateAction::TerminalAccept => self.terminal_accept_push(tx),
            UpdateAction::TerminalDiscard => self.terminal_discard(tx),
        }
    }

    /// Spawn the finalize PTY that commits the opencode-resolved merge once
    /// the user presses **Complete**. The Update Pull Request flow commits
    /// and pushes; the "Update branch (locally)" flow (`local_only`) commits
    /// without pushing. Either way the PTY exit flips the screen onto the
    /// ✅ done page.
    fn start_commit_after_ai(&mut self) {
        let dashboard_config = self.current_dashboard_config();
        let Some(screen) = self.update_pr.as_mut() else {
            return;
        };
        let request = screen.request().clone();
        let model = dashboard_config.ai.update.model.clone();
        let base_ref = request
            .base_ref
            .clone()
            .unwrap_or_else(|| "upstream/main".to_string());
        let cwd = PathBuf::from(&request.worktree_path);
        let message = format!(
            "{}\n\nMerged `{base_ref}` and resolved conflicts using opencode ({model}).",
            crate::constants::UPDATE_MERGE_COMMIT_MESSAGE
        );
        let script = if screen.local_only() {
            "git add -A && git commit -m \"$COMMIT_MSG\"".to_string()
        } else {
            "git add -A && git commit -m \"$COMMIT_MSG\" && git push origin HEAD".to_string()
        };
        let sh = PathBuf::from("/bin/sh");
        let (shell, shell_args) = login_shell_command(&sh, &["-c".to_string(), script]);
        screen.start_commit_push_pty(
            shell,
            shell_args,
            cwd,
            vec![("COMMIT_MSG".to_string(), message)],
        );
    }

    /// Shared dispatch for confirming the update/push confirmation dialog.
    /// Push-only screens re-route to the push pipeline; everything else
    /// runs the full fetch/merge/push update.
    fn confirm_update_pr(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let Some(screen) = self.update_pr.as_mut() else {
            return;
        };
        let request = screen.request().clone();
        let push_only = screen.is_push_only();
        screen.start_updating();
        if push_only {
            screen.set_phase_message("Pushing to origin...");
            kick_off_push_pull_request(
                self.git_root.clone(),
                self.current_dashboard_config(),
                request,
                tx.clone(),
            );
        } else {
            kick_off_update_pull_request(
                self.git_root.clone(),
                self.current_dashboard_config(),
                request,
                tx.clone(),
            );
        }
    }

    /// The user pressed Accept in the Terminal Activity recovery panel:
    /// re-run `git push origin HEAD` and report the real outcome. A repeat
    /// failure simply re-opens the terminal (via `apply_update_pr_finished`).
    fn terminal_accept_push(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let Some(screen) = self.update_pr.as_mut() else {
            return;
        };
        let request = screen.request().clone();
        screen.set_phase_message("Re-attempting push...");
        kick_off_push_pull_request(
            self.git_root.clone(),
            self.current_dashboard_config(),
            request,
            tx.clone(),
        );
    }

    /// The user pressed Discard/Esc in the Terminal Activity recovery panel:
    /// leave the worktree as-is (the local merge is intact) and return to the
    /// dashboard with an explanatory toast.
    fn terminal_discard(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let number = self.update_pr.as_ref().map(|s| s.request().number);
        self.update_pr = None;
        self.enter_screen(Screen::Dashboard, tx);
        if let Some(number) = number {
            self.show_toast(
                ToastVariant::Warning,
                format!(
                    "Left PR #{number} without confirming a push — the local merge is \
                     intact; push when ready."
                ),
            );
        }
    }

    fn handle_explain_pr_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.explain_pr.as_mut() {
            Some(screen) => screen.handle_key(key),
            None => return,
        };
        self.apply_explain_action(action, tx);
    }

    /// Single handler for `ExplainAction`s arriving from either keyboard or
    /// mouse. Drives the screen transitions and kicks off the async pipeline
    /// stages (prepare → spawn opencode → submit).
    fn apply_explain_action(
        &mut self,
        action: ExplainAction,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        match action {
            ExplainAction::Continue => {}
            ExplainAction::Cancelled => {
                let worktree_path = self
                    .explain_pr
                    .take()
                    .map(|s| s.request().worktree_path.clone());
                self.back_to_dashboard_action_menu(worktree_path, tx);
            }
            ExplainAction::Confirmed => {
                let Some(screen) = self.explain_pr.as_mut() else {
                    return;
                };
                let request = screen.request().clone();
                screen.start_explaining();
                kick_off_prepare_explain(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    request,
                    tx.clone(),
                );
            }
            ExplainAction::ReadyToReview => self.on_explain_ready_to_review(tx),
            ExplainAction::Submit => {
                let Some(screen) = self.explain_pr.as_mut() else {
                    return;
                };
                let request = screen.request().clone();
                let Some(title) = screen.draft_title().map(str::to_string) else {
                    self.explain_pr = None;
                    self.enter_screen(Screen::Dashboard, tx);
                    return;
                };
                let submit = ExplainSubmitRequest {
                    worktree_path: request.worktree_path.clone(),
                    branch: request.branch.clone(),
                    number: request.number,
                    base_ref: request.base_ref.clone(),
                    title,
                    body: screen.draft_body().map(str::to_string).unwrap_or_default(),
                    labels: screen.draft_labels().to_vec(),
                    existing_title: request.title.clone(),
                    existing_labels: request.existing_labels.clone(),
                };
                screen.start_opening();
                kick_off_submit_pull_request(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    submit,
                    tx.clone(),
                );
            }
            ExplainAction::Finish => {
                self.show_toast(
                    ToastVariant::Info,
                    "Draft saved to pull_request.md — no pull request was opened.".to_string(),
                );
                self.explain_pr = None;
                self.enter_screen(Screen::Dashboard, tx);
            }
            ExplainAction::Done => {
                self.explain_pr = None;
                self.enter_screen(Screen::Dashboard, tx);
            }
        }
    }

    /// opencode finished drafting: read `pull_request.md` from the worktree,
    /// parse the title + body, and move the screen into Review. A missing or
    /// empty file surfaces an error (the AI likely didn't finish).
    fn on_explain_ready_to_review(&mut self, _tx: &mpsc::UnboundedSender<AppEvent>) {
        let Some(screen) = self.explain_pr.as_mut() else {
            return;
        };
        let path = PathBuf::from(&screen.request().worktree_path).join("pull_request.md");
        match std::fs::read_to_string(&path) {
            Ok(content) => match parse_pull_request_md(&content) {
                Some((title, body, labels)) => screen.enter_review(title, body, labels),
                None => screen.set_error(
                    "pull_request.md has no title line yet — let opencode finish, then retry."
                        .to_string(),
                ),
            },
            Err(_) => screen.set_error(format!(
                "pull_request.md not found at {}. Wait for opencode to write it before confirming.",
                path.display()
            )),
        }
    }

    /// The Explain turn watcher fired — advance exactly like a PTY exit
    /// would (reading `pull_request.md` off disk), just without requiring
    /// the user to quit opencode or confirm the draft is ready themselves.
    /// Clears the watcher immediately on a terminal outcome — `set_error`
    /// does not change the screen's step, so the tick loop's `is_explaining`
    /// gate alone would keep re-polling (and re-erroring) every second.
    fn on_explain_turn(&mut self, turn: OpencodeTurn, tx: &mpsc::UnboundedSender<AppEvent>) {
        match turn {
            OpencodeTurn::Working => {}
            OpencodeTurn::Finished { .. } => {
                self.explain_draft = None;
                self.on_explain_ready_to_review(tx);
            }
            OpencodeTurn::Failed { message } => {
                self.explain_draft = None;
                if let Some(screen) = self.explain_pr.as_mut() {
                    screen.set_error(format!("opencode reported an error: {message}"));
                }
            }
        }
    }

    // ── "Fix Pull Request" orchestration ───────────────────────────────

    fn start_fix_pr_flow(
        &mut self,
        request: FixPullRequestRequest,
        _tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        // Lands on the Confirm step immediately — no base ref to resolve. The
        // prepare pipeline only runs once the user confirms.
        let ai = self.current_dashboard_config().ai.fix.clone();
        self.fix_pr = Some(FixPullRequestScreen::new(request, ai));
        self.screen = Screen::FixPullRequest;
    }

    fn handle_fix_pr_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.fix_pr.as_mut() {
            Some(screen) => screen.handle_key(key),
            None => return,
        };
        self.apply_fix_action(action, tx);
    }

    /// Single handler for `FixAction`s from keyboard or mouse. Drives the
    /// screen transitions and kicks off each async stage of the per-comment
    /// loop (prepare → plan → apply → commit/reply → push).
    fn apply_fix_action(&mut self, action: FixAction, tx: &mpsc::UnboundedSender<AppEvent>) {
        match action {
            FixAction::Continue => {}
            FixAction::Cancelled => {
                let worktree_path = self
                    .fix_pr
                    .take()
                    .map(|s| s.request().worktree_path.clone());
                self.back_to_dashboard_action_menu(worktree_path, tx);
            }
            FixAction::Confirmed => {
                let Some(screen) = self.fix_pr.as_mut() else {
                    return;
                };
                let number = screen.request().number;
                let worktree_path = screen.request().worktree_path.clone();
                screen.start_preparing();
                kick_off_prepare_fix(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    worktree_path,
                    number,
                    tx.clone(),
                );
            }
            FixAction::Apply => {
                let Some(screen) = self.fix_pr.as_mut() else {
                    return;
                };
                let (Some(group), Some(plan)) = (screen.current_group(), screen.current_plan())
                else {
                    return;
                };
                let index = screen.current_index();
                let worktree_path = screen.request().worktree_path.clone();
                screen.start_applying();
                kick_off_prepare_apply(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    FixApplyRequest {
                        worktree_path,
                        group,
                        plan,
                        index,
                    },
                    tx.clone(),
                );
            }
            FixAction::Other => {
                if let Some(screen) = self.fix_pr.as_mut() {
                    screen.show_other_input();
                }
            }
            FixAction::Skip => {
                if let Some(screen) = self.fix_pr.as_mut() {
                    screen.record_outcome(FixRowOutcome::Skipped("you skipped"));
                }
                self.advance_fix(tx);
            }
            FixAction::Replan(feedback) => {
                let previous_plan = self.fix_pr.as_ref().and_then(|s| s.previous_plan_text());
                self.plan_current_fix(tx, Some(feedback), previous_plan);
            }
            FixAction::ApplyReady => self.on_fix_apply_done(tx),
            FixAction::Done => {
                self.fix_pr = None;
                self.enter_screen(Screen::Dashboard, tx);
            }
        }
    }

    /// Plan (or re-plan, when `feedback` is set) the current comment group.
    fn plan_current_fix(
        &mut self,
        tx: &mpsc::UnboundedSender<AppEvent>,
        feedback: Option<String>,
        previous_plan: Option<String>,
    ) {
        let Some(screen) = self.fix_pr.as_mut() else {
            return;
        };
        let Some(group) = screen.current_group() else {
            return;
        };
        let index = screen.current_index();
        let total = screen.groups_len();
        let worktree_path = screen.request().worktree_path.clone();
        let history = screen.history_text();
        screen.start_planning(index + 1, total);
        kick_off_plan_comment(
            self.git_root.clone(),
            self.current_dashboard_config(),
            FixPlanRequest {
                worktree_path,
                group,
                feedback,
                previous_plan,
                history,
                index,
            },
            tx.clone(),
        );
    }

    /// Advance to the next comment group, or push + finish when the loop ends.
    /// A failure on one comment never aborts the loop — it's already recorded
    /// as a Failed row by the caller.
    fn advance_fix(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let has_next = match self.fix_pr.as_mut() {
            Some(screen) => screen.advance(),
            None => return,
        };
        if has_next {
            self.plan_current_fix(tx, None, None);
        } else if let Some(screen) = self.fix_pr.as_mut() {
            let worktree_path = screen.request().worktree_path.clone();
            screen.start_pushing();
            kick_off_push_fix(
                self.git_root.clone(),
                self.current_dashboard_config(),
                worktree_path,
                tx.clone(),
            );
        }
    }

    /// opencode finished editing: commit the change and reply to the reviewer.
    fn on_fix_apply_done(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let Some(screen) = self.fix_pr.as_mut() else {
            return;
        };
        let (Some(group), Some(plan)) = (screen.current_group(), screen.current_plan()) else {
            return;
        };
        let index = screen.current_index();
        let owner = screen.owner().to_string();
        let repo = screen.repo().to_string();
        let number = screen.request().number;
        let pr_url = screen.request().url.clone();
        let worktree_path = screen.request().worktree_path.clone();
        screen.start_committing();
        kick_off_commit_and_reply(
            self.git_root.clone(),
            self.current_dashboard_config(),
            FixCommitRequest {
                worktree_path,
                owner,
                repo,
                number,
                pr_url,
                comment_index: index + 1,
                index,
                group,
                plan,
            },
            tx.clone(),
        );
    }

    /// Autonomous Fix apply: opencode's turn finished (or errored) in the
    /// database. A finished turn commits + replies; a failed turn records the
    /// error as a Failed row and advances to the next comment.
    fn on_fix_turn(&mut self, turn: OpencodeTurn, tx: &mpsc::UnboundedSender<AppEvent>) {
        match turn {
            OpencodeTurn::Working => {}
            OpencodeTurn::Finished { .. } => {
                self.fix_apply_watch = None;
                self.on_fix_apply_done(tx);
            }
            OpencodeTurn::Failed { message } => {
                self.fix_apply_watch = None;
                if let Some(screen) = self.fix_pr.as_mut() {
                    screen.record_outcome(FixRowOutcome::Failed(format!(
                        "opencode reported an error: {}",
                        truncate_error(&message)
                    )));
                }
                self.advance_fix(tx);
            }
        }
    }

    // ── "Review Pull Request" orchestration ────────────────────────────

    fn start_review_pr_flow(
        &mut self,
        request: ReviewPullRequestRequest,
        _tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        // Lands on the Confirm step immediately. The prepare pipeline only
        // runs once the user confirms.
        let ai = self.current_dashboard_config().ai.review.clone();
        self.review_pr = Some(ReviewPullRequestScreen::new(request, ai));
        self.screen = Screen::ReviewPullRequest;
    }

    fn handle_review_pr_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.review_pr.as_mut() {
            Some(screen) => screen.handle_key(key),
            None => return,
        };
        self.apply_review_action(action, tx);
    }

    /// Single handler for `ReviewAction`s from keyboard or mouse. Drives the
    /// screen transitions and kicks off each async stage of the two loops
    /// (per-file scan, then per-finding walkthrough) plus the summary.
    fn apply_review_action(&mut self, action: ReviewAction, tx: &mpsc::UnboundedSender<AppEvent>) {
        match action {
            ReviewAction::Continue => {}
            ReviewAction::Cancelled => {
                let worktree_path = self
                    .review_pr
                    .take()
                    .map(|s| s.request().worktree_path.clone());
                self.back_to_dashboard_action_menu(worktree_path, tx);
            }
            ReviewAction::Confirmed => {
                let Some(screen) = self.review_pr.as_mut() else {
                    return;
                };
                let number = screen.request().number;
                let worktree_path = screen.request().worktree_path.clone();
                screen.start_preparing();
                kick_off_prepare_review(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    worktree_path,
                    number,
                    tx.clone(),
                );
            }
            ReviewAction::Post => {
                let Some(screen) = self.review_pr.as_mut() else {
                    return;
                };
                let Some(finding) = screen.current_finding() else {
                    return;
                };
                let request = ReviewPostRequest {
                    worktree_path: screen.request().worktree_path.clone(),
                    owner: screen.owner().to_string(),
                    repo: screen.repo().to_string(),
                    number: screen.request().number,
                    head_sha: screen.head_sha().to_string(),
                    finding,
                    index: screen.current_index(),
                };
                screen.start_posting();
                kick_off_post_review_finding(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    request,
                    tx.clone(),
                );
            }
            ReviewAction::Other => {
                if let Some(screen) = self.review_pr.as_mut() {
                    screen.show_other_input();
                }
            }
            ReviewAction::Skip => {
                if let Some(screen) = self.review_pr.as_mut() {
                    screen.record_outcome(ReviewRowOutcome::Skipped);
                }
                self.advance_review_finding(tx);
            }
            ReviewAction::Revise(feedback) => {
                let Some(screen) = self.review_pr.as_mut() else {
                    return;
                };
                let Some(finding) = screen.current_finding() else {
                    return;
                };
                // The dedicated revision prompt receives only focused context
                // around this finding, independent of merged/split routing.
                let Some(file) = screen.file_for(&finding) else {
                    screen.reshow_decision();
                    return;
                };
                let request = ReviewReviseRequest {
                    worktree_path: screen.request().worktree_path.clone(),
                    file,
                    finding,
                    mode: if review_feedback_needs_expanded_context(&feedback) {
                        ReviewRevisionMode::Expanded
                    } else {
                        ReviewRevisionMode::Focused
                    },
                    feedback,
                    index: screen.current_index(),
                };
                screen.start_revising();
                kick_off_revise_review_finding(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    request,
                    tx.clone(),
                );
            }
            ReviewAction::SubmitSummary { request_changes } => {
                let Some(screen) = self.review_pr.as_mut() else {
                    return;
                };
                let worktree_path = screen.request().worktree_path.clone();
                let number = screen.request().number;
                let body = screen.summary_body().to_string();
                screen.start_submitting_summary();
                kick_off_submit_review_summary(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    worktree_path,
                    number,
                    body,
                    request_changes,
                    tx.clone(),
                );
            }
            ReviewAction::SkipSummary => {
                if let Some(screen) = self.review_pr.as_mut() {
                    screen.enter_done();
                }
            }
            ReviewAction::Done => {
                self.review_pr = None;
                self.enter_screen(Screen::Dashboard, tx);
            }
        }
    }

    /// Start the scan phase: dispatch up to `REVIEW_SCAN_CONCURRENCY`
    /// per-file scans at once; each completion hands the next pending file
    /// to the freed slot.
    fn start_review_scans(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        if let Some(screen) = self.review_pr.as_mut() {
            screen.begin_scan_phase();
        }
        for _ in 0..REVIEW_SCAN_CONCURRENCY {
            if !self.dispatch_next_review_scan(tx) {
                break;
            }
        }
        self.settle_review_scans(tx);
    }

    /// Hand the next un-dispatched scan to a task: the files first, then the
    /// whole-diff coverage pass. `false` once everything has been dispatched.
    fn dispatch_next_review_scan(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) -> bool {
        let Some(screen) = self.review_pr.as_mut() else {
            return false;
        };
        let worktree_path = screen.request().worktree_path.clone();
        let context = screen.review_context();
        if let Some((file_index, group)) = screen.take_next_scan_file() {
            kick_off_scan_review_file(
                self.git_root.clone(),
                self.current_dashboard_config(),
                ReviewScanRequest {
                    worktree_path,
                    group,
                    context,
                    file_index,
                    retry: ReviewScanRetry::Initial,
                    raw_output: None,
                },
                tx.clone(),
            );
            return true;
        }
        let Some((scan_index, files)) = screen.take_coverage_scan() else {
            return false;
        };
        let mode = screen.scan_mode();
        let context = screen.review_context();
        let tester_findings = screen.tester_findings();
        kick_off_scan_review_coverage(
            self.git_root.clone(),
            self.current_dashboard_config(),
            ReviewCoverageScanRequest {
                worktree_path,
                files,
                scan_index,
                mode,
                context,
                tester_findings,
                retry: ReviewScanRetry::Initial,
                raw_output: None,
            },
            tx.clone(),
        );
        true
    }

    /// Re-kick one scan after unparseable output — its slot in the pool
    /// stays occupied, so no extra scan starts.
    fn retry_review_scan(
        &mut self,
        file_index: usize,
        retry: ReviewScanRetry,
        raw_output: Option<String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let Some(screen) = self.review_pr.as_mut() else {
            return;
        };
        let worktree_path = screen.request().worktree_path.clone();
        let context = screen.review_context();
        if let Some(files) = screen.coverage_group(file_index) {
            let mode = screen.scan_mode();
            let tester_findings = screen.tester_findings();
            kick_off_scan_review_coverage(
                self.git_root.clone(),
                self.current_dashboard_config(),
                ReviewCoverageScanRequest {
                    worktree_path,
                    files,
                    scan_index: file_index,
                    mode,
                    context,
                    tester_findings,
                    retry,
                    raw_output,
                },
                tx.clone(),
            );
            return;
        }
        let Some(group) = screen.scan_group(file_index) else {
            return;
        };
        kick_off_scan_review_file(
            self.git_root.clone(),
            self.current_dashboard_config(),
            ReviewScanRequest {
                worktree_path,
                group,
                context,
                file_index,
                retry,
                raw_output,
            },
            tx.clone(),
        );
    }

    /// Close the scan phase once every file reached a terminal state: sort
    /// the findings and enter the walkthrough (or jump straight to Done on
    /// a clean review).
    fn settle_review_scans(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let Some(screen) = self.review_pr.as_mut() else {
            return;
        };
        if screen.scans_pending() {
            return;
        }
        if screen.should_run_gap_audit() {
            screen.begin_gap_audit();
            let (worktree_path, files, context, relationship_edges, skipped, findings) =
                screen.gap_audit_inputs();
            kick_off_review_gap_audit(
                self.git_root.clone(),
                self.current_dashboard_config(),
                ReviewGapAuditRequest {
                    worktree_path,
                    files,
                    context,
                    relationship_edges,
                    skipped,
                    findings,
                },
                tx.clone(),
            );
            return;
        }
        if screen.finish_scanning() {
            let worktree_path = screen.request().worktree_path.clone();
            let context = screen.review_context();
            let candidates = screen.begin_verification();
            if candidates.is_empty() {
                screen.enter_decision();
                return;
            }
            let config = self.current_dashboard_config();
            for (index, file, finding, strong) in candidates {
                kick_off_verify_review_finding(
                    self.git_root.clone(),
                    config.clone(),
                    ReviewVerifyRequest {
                        worktree_path: worktree_path.clone(),
                        file,
                        finding,
                        context: context.clone(),
                        index,
                        strong,
                    },
                    tx.clone(),
                );
            }
        } else {
            screen.enter_done();
        }
    }

    /// Advance the walkthrough to the next finding, or close it: with posted
    /// comments the deterministic summary is shown, otherwise straight to
    /// the final report.
    fn advance_review_finding(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let _ = tx;
        let Some(screen) = self.review_pr.as_mut() else {
            return;
        };
        if screen.advance_finding() {
            screen.enter_decision();
        } else if screen.posted_findings().is_empty() {
            screen.enter_done();
        } else {
            let body = build_review_summary(screen.posted_findings());
            screen.enter_summary(body);
        }
    }

    fn fail_review(
        &mut self,
        variant: ToastVariant,
        message: String,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        self.show_toast(variant, message);
        self.review_pr = None;
        self.enter_screen(Screen::Dashboard, tx);
    }

    fn apply_review_pr_prepared(
        &mut self,
        result: Result<Box<ReviewPreparation>, String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        if self.review_pr.is_none() {
            return;
        }
        match result {
            Ok(prep) => match *prep {
                ReviewPreparation::Ready {
                    files,
                    scan_mode,
                    context,
                    skipped,
                    owner,
                    repo,
                    head_sha,
                } => {
                    if let Some(screen) = self.review_pr.as_mut() {
                        screen.set_scan_mode(scan_mode);
                        screen.set_review_context(context);
                        screen.set_files(files, owner, repo, head_sha);
                        screen.record_skipped_files(&skipped);
                    }
                    // With every changed file filtered out (e.g. a
                    // lockfile-only PR) this goes straight to the Done
                    // report, which lists each skip and its reason.
                    self.start_review_scans(tx);
                }
                ReviewPreparation::NoChanges => self.fail_review(
                    ToastVariant::Info,
                    "This PR has no reviewable text changes.".to_string(),
                    tx,
                ),
                ReviewPreparation::GhUnavailable => self.fail_review(
                    ToastVariant::Error,
                    "gh CLI not found — install `gh` and run `gh auth login` to review pull \
                     requests."
                        .to_string(),
                    tx,
                ),
                ReviewPreparation::AiNotConfigured => self.fail_review(
                    ToastVariant::Warning,
                    "Set both Review discovery profiles (`strong` and `balanced`) in Settings → \
                     Dashboard → ai so the AI can scan the diff."
                        .to_string(),
                    tx,
                ),
                ReviewPreparation::AiUnavailable => self.fail_review(
                    ToastVariant::Error,
                    "`opencode` CLI is not on PATH — install it from https://opencode.ai then \
                     retry."
                        .to_string(),
                    tx,
                ),
                ReviewPreparation::SyncFailed(err) => self.fail_review(
                    ToastVariant::Error,
                    format!("Could not prepare the review: {}", truncate_error(&err)),
                    tx,
                ),
            },
            Err(message) => self.fail_review(
                ToastVariant::Error,
                format!("Failed to fetch the PR diff: {}", truncate_error(&message)),
                tx,
            ),
        }
    }

    fn apply_review_pr_scanned(
        &mut self,
        file_index: usize,
        retry: ReviewScanRetry,
        result: Result<Vec<ReviewFinding>, String>,
        mut telemetry: Option<ReviewScanTelemetry>,
        raw_output: Option<String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        // Guard against a late arrival after the user cancelled or the scan
        // phase already closed.
        let active = self
            .review_pr
            .as_ref()
            .is_some_and(|s| s.scan_phase_active());
        if !active {
            return;
        }
        if let Some(telemetry) = telemetry.as_mut() {
            telemetry.retry_role = match retry {
                ReviewScanRetry::Initial => "initial",
                ReviewScanRetry::Reformat => "reformat",
                ReviewScanRetry::Full => "full-rescan",
            }
            .to_string();
        }
        if let (Some(screen), Some(telemetry)) = (self.review_pr.as_mut(), telemetry) {
            screen.record_scan_telemetry(telemetry);
        }
        match result {
            Ok(findings) => {
                if let Some(screen) = self.review_pr.as_mut() {
                    screen.record_tester_findings(file_index, &findings);
                    // Deterministic dedup: findings the PR already carries
                    // as a wisetree comment never re-enter the walkthrough,
                    // regardless of whether the model honored the
                    // existing-comments instruction.
                    let (fresh, duplicates) =
                        screen.split_existing_duplicates(file_index, findings);
                    screen.record_duplicate_findings(&duplicates);
                    screen.record_scan_result(fresh);
                    screen.note_scan_done(file_index);
                }
                if !self.dispatch_next_review_scan(tx) {
                    self.settle_review_scans(tx);
                }
            }
            Err(message) => {
                if let Some(next) = next_review_retry(retry, raw_output.is_some()) {
                    let raw_output = (next == ReviewScanRetry::Reformat)
                        .then_some(raw_output)
                        .flatten();
                    self.retry_review_scan(file_index, next, raw_output, tx);
                    return;
                }
                if let Some(screen) = self.review_pr.as_mut() {
                    screen.record_scan_failure(file_index, truncate_error(&message));
                    screen.note_scan_done(file_index);
                }
                if !self.dispatch_next_review_scan(tx) {
                    self.settle_review_scans(tx);
                }
            }
        }
    }

    fn apply_review_pr_revised(
        &mut self,
        index: usize,
        mode: ReviewRevisionMode,
        feedback: String,
        result: Result<Vec<ReviewFinding>, String>,
        telemetry: Option<ReviewScanTelemetry>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        if !self.review_at_index(index) {
            return;
        }
        if let (Some(screen), Some(telemetry)) = (self.review_pr.as_mut(), telemetry) {
            screen.record_scan_telemetry(telemetry);
        }
        // A revision must always return to the Decision screen — with the
        // revised comment when the model obeyed, otherwise with the previous
        // one so the user keeps their place and can refine their feedback.
        match result {
            Ok(findings) if !findings.is_empty() => {
                if let Some(screen) = self.review_pr.as_mut() {
                    let mut revised = findings.into_iter().next().expect("non-empty checked");
                    // A revision can never migrate to another file.
                    revised.file = screen
                        .current_finding()
                        .map(|f| f.file)
                        .unwrap_or(revised.file);
                    screen.show_revised(revised);
                }
            }
            other => {
                if mode == ReviewRevisionMode::Focused {
                    let request = self.review_pr.as_ref().and_then(|screen| {
                        let finding = screen.current_finding()?;
                        let file = screen.file_for(&finding)?;
                        Some(ReviewReviseRequest {
                            worktree_path: screen.request().worktree_path.clone(),
                            file,
                            finding,
                            feedback: feedback.clone(),
                            mode: ReviewRevisionMode::Expanded,
                            index,
                        })
                    });
                    if let Some(request) = request {
                        kick_off_revise_review_finding(
                            self.git_root.clone(),
                            self.current_dashboard_config(),
                            request,
                            tx.clone(),
                        );
                        return;
                    }
                }
                if let Some(screen) = self.review_pr.as_mut() {
                    screen.reshow_decision();
                }
                let message = match other {
                    Err(msg) => format!("Could not revise the comment: {}", truncate_error(&msg)),
                    _ => "The AI didn't return a revised comment — kept the previous one. \
                          Adjust your feedback and try Other again."
                        .to_string(),
                };
                self.show_toast(ToastVariant::Warning, message);
            }
        }
    }

    fn apply_review_pr_verified(
        &mut self,
        index: usize,
        result: Result<ReviewVerification, String>,
        telemetry: Option<ReviewScanTelemetry>,
    ) {
        let Some(screen) = self.review_pr.as_mut() else {
            return;
        };
        if let Some(telemetry) = telemetry {
            screen.record_scan_telemetry(telemetry);
        }
        screen.record_verification(index, result);
        if screen.verification_pending() {
            return;
        }
        if screen.finish_verification() {
            screen.enter_decision();
        } else {
            screen.enter_done();
        }
    }

    fn apply_review_pr_gap_audited(
        &mut self,
        result: Result<Vec<ReviewFinding>, String>,
        telemetry: Option<ReviewScanTelemetry>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let Some(screen) = self.review_pr.as_mut() else {
            return;
        };
        if let Some(telemetry) = telemetry {
            screen.record_scan_telemetry(telemetry);
        }
        screen.record_gap_audit_result(result);
        self.settle_review_scans(tx);
    }

    fn apply_review_pr_posted(
        &mut self,
        index: usize,
        result: Result<(), String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        if !self.review_at_index(index) {
            return;
        }
        if let Some(screen) = self.review_pr.as_mut() {
            match result {
                Ok(()) => screen.record_outcome(ReviewRowOutcome::Posted),
                Err(msg) => screen.record_outcome(ReviewRowOutcome::Failed(format!(
                    "post failed: {}",
                    truncate_error(&msg)
                ))),
            }
        }
        self.advance_review_finding(tx);
    }

    fn apply_review_pr_summary_submitted(
        &mut self,
        request_changes: bool,
        result: Result<(), String>,
    ) {
        if let Some(screen) = self.review_pr.as_mut() {
            screen.record_summary_outcome(request_changes, result.map_err(|e| truncate_error(&e)));
            screen.enter_done();
        }
    }

    /// `true` when the review screen is still on finding `index` — guards
    /// every async result against a late arrival after the user moved on.
    fn review_at_index(&self, index: usize) -> bool {
        self.review_pr
            .as_ref()
            .is_some_and(|s| s.current_index() == index)
    }

    // ── "Bugkill" orchestration ─────────────────────────────────────────

    fn start_bugkill_flow(
        &mut self,
        request: BugkillRequest,
        _tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        // Lands on the Confirm step immediately; the preflight only runs
        // once the user has confirmed and described the bug.
        let ai = self.current_dashboard_config().ai.bugkill.clone();
        self.bugkill_pr = Some(BugkillPullRequestScreen::new(request, ai));
        self.screen = Screen::BugkillPullRequest;
    }

    fn handle_bugkill_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.bugkill_pr.as_mut() {
            Some(screen) => screen.handle_key(key),
            None => return,
        };
        self.apply_bugkill_action(action, tx);
    }

    /// Single handler for `BugkillAction`s from keyboard or mouse. Drives
    /// the screen transitions and kicks off each async stage; all loops,
    /// questions, and git operations stay in Rust (invariant I4).
    fn apply_bugkill_action(
        &mut self,
        action: BugkillAction,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        match action {
            BugkillAction::Continue => {}
            BugkillAction::Cancelled => {
                let worktree_path = self
                    .bugkill_pr
                    .take()
                    .map(|s| s.request().worktree_path.clone());
                self.bugkill_investigation = None;
                self.back_to_dashboard_action_menu(worktree_path, tx);
            }
            BugkillAction::Confirmed => {
                // Preflight first: when a resumable BUG_INVESTIGATION.md
                // exists, the Resume prompt already carries the description —
                // asking for it again would be wasted effort. DescribeBug is
                // only shown for the start-fresh paths.
                let Some(screen) = self.bugkill_pr.as_mut() else {
                    return;
                };
                let worktree_path = screen.request().worktree_path.clone();
                screen.start_working("Preparing...", false);
                kick_off_bugkill_preflight(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    worktree_path,
                    tx.clone(),
                );
            }
            BugkillAction::DescriptionSubmitted(description) => {
                let Some(screen) = self.bugkill_pr.as_mut() else {
                    return;
                };
                screen.set_bug_description(description);
                self.start_bugkill_investigation(false, tx);
            }
            BugkillAction::DiscardLeftovers => {
                let Some(screen) = self.bugkill_pr.as_mut() else {
                    return;
                };
                let worktree_path = screen.request().worktree_path.clone();
                let paths = screen.leftover_tracked();
                screen.start_working("Discarding leftover changes...", true);
                kick_off_bugkill_discard(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    worktree_path,
                    paths,
                    tx.clone(),
                );
            }
            BugkillAction::Resume => {
                let Some(screen) = self.bugkill_pr.as_mut() else {
                    return;
                };
                match screen.apply_resume() {
                    // An applied-but-unanswered attempt must be resolved
                    // before anything else — re-ask the Verdict question.
                    Some(unverdicted) => screen.enter_verdict_for_resume(unverdicted),
                    None => {
                        screen.enter_select();
                    }
                }
            }
            BugkillAction::StartFresh => {
                // Fresh run over an existing (or unparseable) investigation
                // file — the description still has to be collected.
                if let Some(screen) = self.bugkill_pr.as_mut() {
                    screen.show_describe();
                }
            }
            BugkillAction::ForceInvestigationDone => self.force_bugkill_investigation_done(tx),
            BugkillAction::AttemptFix => self.start_bugkill_attempt(None, tx),
            BugkillAction::AbortFix => {
                let Some(screen) = self.bugkill_pr.as_mut() else {
                    return;
                };
                let Some(pre) = screen.pre_snapshot() else {
                    return;
                };
                let worktree_path = screen.request().worktree_path.clone();
                screen.kill_pty();
                screen.start_working("Rolling back the attempt...", true);
                kick_off_bugkill_abort(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    worktree_path,
                    pre,
                    tx.clone(),
                );
            }
            BugkillAction::FixFinished => self.on_bugkill_fix_done(tx),
            BugkillAction::VerdictYes => {
                let Some(screen) = self.bugkill_pr.as_mut() else {
                    return;
                };
                // The attempt commit *is* the delivered fix — nothing
                // further happens in git (no push).
                screen.mark_worked(true);
                self.rewrite_bugkill_file(tx);
                if let Some(screen) = self.bugkill_pr.as_mut() {
                    screen.enter_done(true);
                }
            }
            BugkillAction::VerdictNo | BugkillAction::RollbackAndChoose => {
                self.rollback_bugkill_attempt(tx)
            }
            BugkillAction::OtherSubmitted(text) => {
                let Some(screen) = self.bugkill_pr.as_mut() else {
                    return;
                };
                let Some(row) = screen.current_row() else {
                    return;
                };
                let worktree_path = screen.request().worktree_path.clone();
                screen.start_working("Judging the outcome...", false);
                kick_off_bugkill_judge(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    worktree_path,
                    row,
                    text,
                    tx.clone(),
                );
            }
            BugkillAction::RetryWithFeedback => {
                // Re-enter the fix phase for the same row, without reverting:
                // the previous edits stay committed and the retry's edits are
                // folded into the same attempt commit via --amend.
                let feedback = self.bugkill_pr.as_ref().and_then(|s| s.attempt_feedback());
                self.start_bugkill_attempt(feedback, tx);
            }
            BugkillAction::Done => {
                self.bugkill_pr = None;
                self.enter_screen(Screen::Dashboard, tx);
            }
        }
    }

    /// Kick off one live investigation: build the opencode TUI spawn
    /// params, then show it in the embedded PTY. `corrective` is only set
    /// on the automatic retry after an unparseable transcript.
    fn start_bugkill_investigation(
        &mut self,
        corrective: bool,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        self.bugkill_investigation = None;
        let Some(screen) = self.bugkill_pr.as_mut() else {
            return;
        };
        let worktree_path = screen.request().worktree_path.clone();
        let description = screen.bug_description().to_string();
        let base_ref = screen.base_ref().map(str::to_string);
        screen.start_working("Preparing the investigation...", false);
        kick_off_bugkill_prepare_investigate(
            self.git_root.clone(),
            self.current_dashboard_config(),
            worktree_path,
            description,
            base_ref,
            corrective,
            tx.clone(),
        );
    }

    /// Kick off one fix attempt for the targeted row: fresh untracked
    /// snapshot + opencode spawn params. `feedback` is only set on a
    /// retry-with-feedback re-entry.
    fn start_bugkill_attempt(
        &mut self,
        feedback: Option<String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let Some(screen) = self.bugkill_pr.as_mut() else {
            return;
        };
        let Some(row_index) = screen.attempt_target_index() else {
            return;
        };
        let Some(row) = screen.current_row() else {
            return;
        };
        let worktree_path = screen.request().worktree_path.clone();
        let bug_description = screen.bug_description().to_string();
        screen.start_working("Preparing the fix...", false);
        kick_off_bugkill_prepare_fix(
            self.git_root.clone(),
            self.current_dashboard_config(),
            BugkillPrepareFixRequest {
                worktree_path,
                bug_description,
                row,
                row_index,
                feedback,
            },
            tx.clone(),
        );
    }

    /// opencode finished (exited or the user confirmed): scan the worktree
    /// against the pre-attempt snapshot and commit (or amend) the attempt.
    fn on_bugkill_fix_done(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let Some(screen) = self.bugkill_pr.as_mut() else {
            return;
        };
        let Some(pre) = screen.pre_snapshot() else {
            return;
        };
        let Some(row) = screen.current_row() else {
            return;
        };
        let amend = screen.attempt_feedback().is_some();
        let worktree_path = screen.request().worktree_path.clone();
        screen.kill_pty();
        screen.start_working("Scanning the attempt...", true);
        kick_off_bugkill_commit(
            self.git_root.clone(),
            self.current_dashboard_config(),
            BugkillCommitRequest {
                worktree_path,
                pre,
                number: row.number,
                solution: row.solution,
                amend,
            },
            tx.clone(),
        );
    }

    /// History-preserving rollback of the current attempt (the No path).
    /// The revert runs *first*; only after it succeeds is the row marked
    /// failed and the file rewritten (crash-safety ordering). A resume-
    /// recovered attempt without an identified commit skips the revert and
    /// records a note instead.
    fn rollback_bugkill_attempt(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let Some(screen) = self.bugkill_pr.as_mut() else {
            return;
        };
        match screen.attempt_sha() {
            Some(sha) => {
                let worktree_path = screen.request().worktree_path.clone();
                screen.start_working("Rolling back the attempt...", true);
                kick_off_bugkill_rollback(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    worktree_path,
                    sha,
                    tx.clone(),
                );
            }
            None => {
                screen.note_unidentified_attempt();
                screen.mark_worked(false);
                self.rewrite_bugkill_file(tx);
                if let Some(screen) = self.bugkill_pr.as_mut() {
                    screen.enter_select();
                }
            }
        }
    }

    /// Surface a terminal preflight failure as a toast and return to the
    /// dashboard, dropping the Bugkill screen.
    fn fail_bugkill(
        &mut self,
        variant: ToastVariant,
        message: impl Into<String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        self.show_toast(variant, message.into());
        self.bugkill_pr = None;
        self.enter_screen(Screen::Dashboard, tx);
    }

    /// Re-render `BUG_INVESTIGATION.md` from the in-memory model and write
    /// it to the worktree root (invariant I1: called after every mutation).
    fn rewrite_bugkill_file(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let Some(screen) = self.bugkill_pr.as_ref() else {
            return;
        };
        let path = PathBuf::from(&screen.request().worktree_path)
            .join(crate::services::bugkill::INVESTIGATION_FILE);
        let content = screen.render_investigation();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(err) = tokio::fs::write(&path, content).await {
                let _ = tx.send(AppEvent::BugkillFileWriteFailed(err.to_string()));
            }
        });
    }

    fn apply_bugkill_prepared(
        &mut self,
        result: Result<Box<BugkillPreflightOutcome>, String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        if self.bugkill_pr.is_none() {
            return;
        }
        match result {
            Err(message) => self.fail_bugkill(
                ToastVariant::Error,
                format!("Bugkill preparation failed: {}", truncate_error(&message)),
                tx,
            ),
            Ok(outcome) => match *outcome {
                BugkillPreflightOutcome::AiNotConfigured => self.fail_bugkill(
                    ToastVariant::Warning,
                    "ai.bugkill.investigate model is not configured.",
                    tx,
                ),
                BugkillPreflightOutcome::AiUnavailable => self.fail_bugkill(
                    ToastVariant::Error,
                    "`opencode` CLI is not on PATH — install it from https://opencode.ai then \
                     retry.",
                    tx,
                ),
                BugkillPreflightOutcome::DirtyTree { count } => self.fail_bugkill(
                    ToastVariant::Warning,
                    format!(
                        "{count} uncommitted tracked change(s) in the worktree — commit or \
                         stash them before running Bugkill."
                    ),
                    tx,
                ),
                BugkillPreflightOutcome::LeftoverAttempt { tracked } => {
                    if let Some(screen) = self.bugkill_pr.as_mut() {
                        screen.show_leftover_prompt(tracked);
                    }
                }
                BugkillPreflightOutcome::Ready(preflight) => {
                    let Some(screen) = self.bugkill_pr.as_mut() else {
                        return;
                    };
                    screen.set_base_ref(preflight.base_ref);
                    match preflight.resume {
                        BugkillResumeState::Absent => screen.show_describe(),
                        BugkillResumeState::Unparseable => screen.show_overwrite_prompt(),
                        BugkillResumeState::Parsed {
                            investigation,
                            unverdicted,
                        } => screen.show_resume_prompt(investigation, unverdicted),
                    }
                }
            },
        }
    }

    fn apply_bugkill_discarded(
        &mut self,
        result: Result<(), String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let Some(screen) = self.bugkill_pr.as_mut() else {
            return;
        };
        match result {
            Ok(()) => {
                // Debris gone — continue the preflight from the clean tree.
                let worktree_path = screen.request().worktree_path.clone();
                screen.start_working("Preparing...", false);
                kick_off_bugkill_preflight(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    worktree_path,
                    tx.clone(),
                );
            }
            Err(message) => screen.set_error(format!(
                "could not discard the leftover changes: {}",
                truncate_error(&message)
            )),
        }
    }

    /// Spawn params ready → show the AI Activity panel and launch the
    /// opencode TUI inside the embedded PTY, so the user watches the
    /// investigation exactly as opencode itself renders it. The watcher is
    /// created **before** the spawn so its start timestamp precedes the
    /// session row the TUI creates.
    fn apply_bugkill_investigate_ready(
        &mut self,
        corrective: bool,
        result: Result<Box<FixApplyHandoff>, String>,
    ) {
        let Some(screen) = self.bugkill_pr.as_mut() else {
            return;
        };
        match result {
            Ok(handoff) => {
                self.bugkill_investigation = Some(OpencodeTurnWatcher::new(&handoff.cwd));
                screen.start_investigating(corrective);
                screen.spawn_opencode_pty(
                    handoff.opencode_binary,
                    handoff.opencode_args,
                    handoff.cwd,
                    Vec::new(),
                );
            }
            Err(message) => screen.set_error(message),
        }
    }

    /// The turn watcher fired while `Investigating` — advance on Finished
    /// or Failed, keep waiting on Working.
    fn on_bugkill_turn(&mut self, turn: OpencodeTurn, tx: &mpsc::UnboundedSender<AppEvent>) {
        match turn {
            OpencodeTurn::Working => {}
            OpencodeTurn::Finished { transcript } => {
                self.finish_bugkill_investigation(transcript, tx)
            }
            OpencodeTurn::Failed { message } => {
                self.bugkill_investigation = None;
                if let Some(screen) = self.bugkill_pr.as_mut() {
                    screen.kill_pty();
                    screen.set_error(format!("opencode reported an error: {message}"));
                }
            }
        }
    }

    /// The investigation transcript is in: tear the TUI down and parse the
    /// ranked hypotheses. An unparseable transcript earns one corrective
    /// retry (stricter prompt); a second failure surfaces the tail.
    fn finish_bugkill_investigation(
        &mut self,
        transcript: String,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        self.bugkill_investigation = None;
        let Some(screen) = self.bugkill_pr.as_mut() else {
            return;
        };
        let corrective = screen.investigate_corrective();
        screen.kill_pty();
        match crate::services::parse_hypotheses(&transcript) {
            Some(hypotheses) => {
                screen.set_hypotheses(crate::services::normalize_hypotheses(hypotheses));
                self.rewrite_bugkill_file(tx);
                if let Some(screen) = self.bugkill_pr.as_mut() {
                    screen.enter_select();
                }
            }
            None if !corrective => self.start_bugkill_investigation(true, tx),
            None => screen.set_error(format!(
                "could not parse ranked hypotheses from the investigation output. Raw tail:\n{}",
                crate::services::transcript_tail(&transcript)
            )),
        }
    }

    /// The investigation TUI exited before the watcher saw a completed turn
    /// (the user quit opencode, or it crashed). Check the database once —
    /// the turn may have completed right before the exit — otherwise error.
    fn on_bugkill_investigation_pty_exited(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let turn = self
            .bugkill_investigation
            .as_mut()
            .map(OpencodeTurnWatcher::check_now)
            .unwrap_or(OpencodeTurn::Working);
        match turn {
            OpencodeTurn::Working => {
                self.bugkill_investigation = None;
                if let Some(screen) = self.bugkill_pr.as_mut() {
                    screen.kill_pty();
                    screen.set_error(
                        "opencode exited before the investigation finished.".to_string(),
                    );
                }
            }
            turn => self.on_bugkill_turn(turn, tx),
        }
    }

    /// Enter → "Continue now" confirmed: the user says opencode is done but
    /// the automatic detection has not fired. Re-check once; if the turn
    /// still looks unfinished, try whatever transcript exists — if even the
    /// contract parser accepts it the detection was simply blind (e.g. an
    /// unreadable database), otherwise keep waiting and say so.
    fn force_bugkill_investigation_done(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let Some(watcher) = self.bugkill_investigation.as_mut() else {
            return;
        };
        match watcher.check_now() {
            OpencodeTurn::Working => {
                let transcript = watcher.transcript_now().unwrap_or_default();
                if crate::services::parse_hypotheses(&transcript).is_some() {
                    self.finish_bugkill_investigation(transcript, tx);
                } else if let Some(screen) = self.bugkill_pr.as_mut() {
                    screen.note_investigation_waiting();
                }
            }
            turn => self.on_bugkill_turn(turn, tx),
        }
    }

    fn apply_bugkill_fix_ready(
        &mut self,
        row_index: usize,
        result: Result<Box<(BugkillSnapshot, FixApplyHandoff)>, String>,
    ) {
        let Some(screen) = self.bugkill_pr.as_mut() else {
            return;
        };
        match result {
            Ok(payload) => {
                let (snapshot, handoff) = *payload;
                screen.begin_attempt(row_index, snapshot);
                screen.start_fixing();
                screen.spawn_opencode_pty(
                    handoff.opencode_binary,
                    handoff.opencode_args,
                    handoff.cwd,
                    Vec::new(),
                );
            }
            Err(message) => {
                // Gate failures (fix model unconfigured, opencode missing)
                // return to the table — the row stays eligible.
                self.show_toast(ToastVariant::Error, truncate_error(&message));
                if let Some(screen) = self.bugkill_pr.as_mut() {
                    screen.enter_select();
                }
            }
        }
    }

    fn apply_bugkill_committed(
        &mut self,
        result: Result<BugkillCommitOutcome, String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let Some(screen) = self.bugkill_pr.as_mut() else {
            return;
        };
        match result {
            Ok(BugkillCommitOutcome::NoChanges) => {
                let number = screen.current_row().map(|r| r.number).unwrap_or(0);
                // Do not mark implemented — the row stays eligible.
                screen.enter_select();
                self.show_toast(
                    ToastVariant::Info,
                    format!("The fix AI made no changes for row #{number}."),
                );
            }
            Ok(BugkillCommitOutcome::Committed { sha, changes }) => {
                screen.set_attempt_committed(
                    sha,
                    changes.all,
                    changes.modified_preexisting_untracked,
                );
                self.rewrite_bugkill_file(tx);
                if let Some(screen) = self.bugkill_pr.as_mut() {
                    screen.enter_verdict(None);
                }
            }
            Err(message) => screen.set_error(message),
        }
    }

    fn apply_bugkill_aborted(&mut self, result: Result<(), String>) {
        let Some(screen) = self.bugkill_pr.as_mut() else {
            return;
        };
        match result {
            Ok(()) => {
                screen.enter_select();
                self.show_toast(ToastVariant::Info, "Attempt aborted and rolled back.");
            }
            Err(message) => screen.set_error(format!(
                "could not roll the aborted attempt back: {}",
                truncate_error(&message)
            )),
        }
    }

    fn apply_bugkill_judged(
        &mut self,
        user_text: String,
        result: Result<BugkillVerdict, String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let Some(screen) = self.bugkill_pr.as_mut() else {
            return;
        };
        match result {
            // Gate failure (judge model unconfigured): the answer is still
            // required — back to the question.
            Err(message) => {
                screen.enter_verdict(None);
                self.show_toast(ToastVariant::Error, truncate_error(&message));
            }
            Ok(verdict) => match verdict.result {
                JudgeResult::Fixed => self.apply_bugkill_action(BugkillAction::VerdictYes, tx),
                JudgeResult::NotFixed => screen.show_retry_prompt(user_text),
                JudgeResult::Unclear => {
                    let note = if verdict.reason.trim().is_empty() {
                        "The judge could not tell — please answer Yes or No.".to_string()
                    } else {
                        verdict.reason
                    };
                    screen.enter_verdict(Some(note));
                }
            },
        }
    }

    fn apply_bugkill_rolled_back(
        &mut self,
        result: Result<(), String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let Some(screen) = self.bugkill_pr.as_mut() else {
            return;
        };
        match result {
            Ok(()) => {
                // Only after the revert succeeds does the model record the
                // failure — an interruption between the two leaves the
                // branch clean and just re-asks the verdict on resume.
                screen.mark_worked(false);
                self.rewrite_bugkill_file(tx);
                if let Some(screen) = self.bugkill_pr.as_mut() {
                    screen.enter_select();
                }
            }
            // A failed revert is a hard error, never retried silently.
            Err(message) => screen.set_error(message),
        }
    }

    /// Surface a terminal failure and return to the dashboard, dropping the
    /// fix screen. Shared by every non-recoverable prepare-stage outcome.
    fn fail_fix(
        &mut self,
        variant: ToastVariant,
        message: String,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        self.show_toast(variant, message);
        self.fix_pr = None;
        self.enter_screen(Screen::Dashboard, tx);
    }

    fn apply_fix_pr_prepared(
        &mut self,
        result: Result<Box<FixPreparation>, String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        if self.fix_pr.is_none() {
            return;
        }
        match result {
            Ok(prep) => match *prep {
                FixPreparation::Ready {
                    groups,
                    owner,
                    repo,
                } => {
                    if let Some(screen) = self.fix_pr.as_mut() {
                        screen.set_groups(groups, owner, repo);
                    }
                    self.plan_current_fix(tx, None, None);
                }
                FixPreparation::NoComments => self.fail_fix(
                    ToastVariant::Info,
                    "No unresolved review comments to fix on this PR.".to_string(),
                    tx,
                ),
                FixPreparation::GhUnavailable => self.fail_fix(
                    ToastVariant::Error,
                    "gh CLI not found — install `gh` and run `gh auth login` to fix review \
                     comments."
                        .to_string(),
                    tx,
                ),
                FixPreparation::AiNotConfigured => self.fail_fix(
                    ToastVariant::Warning,
                    "Set the `ai.fix.plan` model (Settings → Dashboard → ai) so the AI can plan review fixes."
                        .to_string(),
                    tx,
                ),
                FixPreparation::AiUnavailable => self.fail_fix(
                    ToastVariant::Error,
                    "`opencode` CLI is not on PATH — install it from https://opencode.ai then \
                     retry."
                        .to_string(),
                    tx,
                ),
                FixPreparation::SyncFailed(err) => self.fail_fix(
                    ToastVariant::Error,
                    format!("Could not sync the branch: {}", truncate_error(&err)),
                    tx,
                ),
            },
            Err(message) => self.fail_fix(
                ToastVariant::Error,
                format!(
                    "Failed to fetch review comments: {}",
                    truncate_error(&message)
                ),
                tx,
            ),
        }
    }

    fn apply_fix_pr_planned(
        &mut self,
        index: usize,
        is_replan: bool,
        result: Result<FixVerdict, String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        if !self.fix_at_index(index) {
            return;
        }
        // An "Other" re-plan revises a `fix` plan the user is actively
        // reviewing, so it must always return to the Decision screen with a
        // revised plan — never post a reply or skip + advance. The planning
        // prompt is told to always emit `fix` here; this guard keeps the loop
        // correct even if the model disobeys or the call fails: we keep the user
        // on the previous proposal and let them refine their feedback.
        if is_replan {
            match result {
                Ok(FixVerdict::Fix(plan)) => {
                    if let Some(s) = self.fix_pr.as_mut() {
                        s.show_decision(plan);
                    }
                }
                other => {
                    if let Some(s) = self.fix_pr.as_mut() {
                        s.reshow_decision();
                    }
                    let message = match other {
                        Err(msg) => {
                            format!("Could not revise the plan: {}", truncate_error(&msg))
                        }
                        _ => "The AI didn't return a revised fix — kept the previous plan. \
                              Adjust your feedback and try Other again."
                            .to_string(),
                    };
                    self.show_toast(ToastVariant::Warning, message);
                }
            }
            return;
        }
        match result {
            Ok(FixVerdict::Praise) => {
                let reaction_info = self.fix_pr.as_mut().and_then(|s| {
                    s.record_outcome(FixRowOutcome::Skipped("praise"));
                    let group = s.current_group()?;
                    Some((
                        s.owner().to_string(),
                        s.repo().to_string(),
                        s.request().worktree_path.clone(),
                        group,
                    ))
                });
                if let Some((owner, repo, worktree_path, group)) = reaction_info {
                    kick_off_praise_reaction(
                        self.git_root.clone(),
                        self.current_dashboard_config(),
                        owner,
                        repo,
                        worktree_path,
                        group,
                    );
                }
                self.advance_fix(tx);
            }
            Ok(FixVerdict::Reply(text)) => {
                let Some(screen) = self.fix_pr.as_mut() else {
                    return;
                };
                let Some(group) = screen.current_group() else {
                    return;
                };
                let owner = screen.owner().to_string();
                let repo = screen.repo().to_string();
                let number = screen.request().number;
                let worktree_path = screen.request().worktree_path.clone();
                screen.set_pending_reply(text.clone());
                screen.start_posting_reply();
                kick_off_post_reply(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    FixReplyRequest {
                        worktree_path,
                        owner,
                        repo,
                        number,
                        group,
                        text,
                        index,
                    },
                    tx.clone(),
                );
            }
            Ok(FixVerdict::Fix(plan)) => {
                let autonomous = self.fix_pr.as_ref().is_some_and(|s| s.autonomous());
                if let Some(s) = self.fix_pr.as_mut() {
                    s.show_decision(plan);
                }
                // Autonomous mode approves the plan for the user: apply it now
                // instead of pausing on the Apply / Other / Skip page. This is
                // exactly the path `FixAction::Apply` drives when the user picks
                // Apply by hand, so the rest of the loop is unchanged.
                if autonomous {
                    self.apply_fix_action(FixAction::Apply, tx);
                }
            }
            Err(msg) => {
                if let Some(s) = self.fix_pr.as_mut() {
                    s.record_outcome(FixRowOutcome::Failed(format!(
                        "planning failed: {}",
                        truncate_error(&msg)
                    )));
                }
                self.advance_fix(tx);
            }
        }
    }

    fn apply_fix_pr_replied(
        &mut self,
        index: usize,
        result: Result<(), String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        if !self.fix_at_index(index) {
            return;
        }
        if let Some(s) = self.fix_pr.as_mut() {
            match result {
                Ok(()) => s.record_outcome(FixRowOutcome::Replied),
                Err(msg) => s.record_outcome(FixRowOutcome::Failed(format!(
                    "reply failed: {}",
                    truncate_error(&msg)
                ))),
            }
        }
        self.advance_fix(tx);
    }

    fn apply_fix_pr_apply_ready(
        &mut self,
        index: usize,
        result: Result<Box<FixApplyHandoff>, String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        if !self.fix_at_index(index) {
            return;
        }
        match result {
            Ok(handoff) => {
                let handoff = *handoff;
                if let Some(s) = self.fix_pr.as_mut() {
                    // In Autonomous mode, watch opencode's database so the
                    // finished turn commits the fix automatically; manual mode
                    // waits for the user's Enter + finalize confirm instead.
                    if s.autonomous() {
                        self.fix_apply_watch = Some(OpencodeTurnWatcher::new(&handoff.cwd));
                    }
                    s.spawn_opencode_pty(
                        handoff.opencode_binary,
                        handoff.opencode_args,
                        handoff.cwd,
                        Vec::new(),
                    );
                }
            }
            Err(msg) => {
                if let Some(s) = self.fix_pr.as_mut() {
                    s.record_outcome(FixRowOutcome::Failed(format!(
                        "could not start the editor: {}",
                        truncate_error(&msg)
                    )));
                }
                self.advance_fix(tx);
            }
        }
    }

    fn apply_fix_pr_committed(
        &mut self,
        index: usize,
        result: Result<FixCommitOutcome, String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        if !self.fix_at_index(index) {
            return;
        }
        if let Some(s) = self.fix_pr.as_mut() {
            match result {
                Ok(FixCommitOutcome::Committed) => s.record_outcome(FixRowOutcome::Applied),
                Ok(FixCommitOutcome::AlreadyResolved) => {
                    s.record_outcome(FixRowOutcome::AlreadyResolved)
                }
                Err(msg) => s.record_outcome(FixRowOutcome::Failed(truncate_error(&msg))),
            }
        }
        self.advance_fix(tx);
    }

    /// `true` when the fix screen is still processing comment `index` — guards
    /// every async result against a late arrival after the user moved on.
    fn fix_at_index(&self, index: usize) -> bool {
        self.fix_pr
            .as_ref()
            .is_some_and(|s| s.current_index() == index)
    }

    fn handle_menu_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        if self.menu.is_none() {
            self.menu = Some(self.build_menu_screen());
        }
        let menu = self.menu.as_mut().expect("menu set above");
        match menu.handle_key(key) {
            MenuOutcome::Selected(choice, idx) => {
                self.last_menu_index = idx;
                match choice {
                    MenuChoice::Exit => self.quit_requested = true,
                    MenuChoice::Setup => self.enter_screen(Screen::Setup, tx),
                    MenuChoice::Create => self.enter_screen(Screen::Create, tx),
                    MenuChoice::Dashboard => self.enter_screen(Screen::Dashboard, tx),
                    MenuChoice::Cache => self.enter_screen(Screen::Cache, tx),
                    MenuChoice::Settings => self.enter_screen(Screen::Settings, tx),
                }
            }
            MenuOutcome::Cancelled => self.quit_requested = true,
            MenuOutcome::Pending => {}
        }
    }

    fn handle_dashboard_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.dashboard.as_mut() {
            Some(dashboard) => dashboard.handle_key(key),
            None => return,
        };
        self.apply_dashboard_action(action, tx);
    }

    fn apply_dashboard_action(
        &mut self,
        action: DashboardAction,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        match action {
            DashboardAction::Continue => {}
            DashboardAction::Back => self.back_to_menu(),
            DashboardAction::Refresh => {
                if let Some(watch) = self.dashboard_watch.as_ref() {
                    watch.refresh();
                }
            }
            DashboardAction::NavigateTo(path) => {
                if self.is_from_wrapper {
                    self.selected_path = Some(path);
                    self.quit_requested = true;
                }
            }
            DashboardAction::OpenTerminal { path, branch } => {
                if let Some(config) = self.current_config() {
                    let variables = self.terminal_template_variables(&path, &branch);
                    let launch = open_terminal(&config.terminal_command, &variables);
                    if launch.success {
                        self.show_toast(
                            ToastVariant::Info,
                            format!("Opened terminal command for {}", fold_path(&path)),
                        );
                    } else if let Some(error) = launch.error {
                        self.show_toast(
                            ToastVariant::Error,
                            format!("Failed to open terminal for {}: {error}", fold_path(&path)),
                        );
                    }
                }
            }
            DashboardAction::JumpToDelete(path) => {
                self.pending_delete_path = Some(path);
                self.enter_screen(Screen::Delete, tx);
            }
            DashboardAction::MotherWorktreeProtected => {
                self.show_toast(
                    ToastVariant::Warning,
                    "The mother worktree is protected and cannot be deleted.",
                );
            }
            DashboardAction::BulkDelete(status, paths) => {
                self.start_bulk_delete_flow(status, paths, tx);
            }
            DashboardAction::UpdateAllBranches(targets) => {
                self.start_update_all_branches(targets, tx);
            }
            DashboardAction::UpdateAllPullRequests(targets) => {
                self.start_update_all_prs(targets, tx);
            }
            DashboardAction::CopyPath(path) => {
                let success_message = format!("Copied {} to clipboard.", fold_path(&path));
                kick_off_clipboard_copy(path, success_message, tx.clone());
            }
            DashboardAction::OpenPullRequest(url) => match open_url(&url) {
                Ok(()) => self.show_toast(ToastVariant::Info, format!("Opened pull request {url}")),
                Err(err) => self.show_toast(
                    ToastVariant::Error,
                    format!("Failed to open pull request: {err}"),
                ),
            },
            DashboardAction::MergePullRequest(request) => {
                self.start_merge_pr_flow(*request, tx);
            }
            DashboardAction::UpdatePullRequest(request) => {
                self.start_update_pr_flow(*request, tx);
            }
            DashboardAction::ExplainPullRequest(request) => {
                self.start_explain_pr_flow(*request, tx);
            }
            DashboardAction::FixPullRequest(request) => {
                self.start_fix_pr_flow(*request, tx);
            }
            DashboardAction::ReviewPullRequest(request) => {
                self.start_review_pr_flow(*request, tx);
            }
            DashboardAction::Bugkill(request) => {
                self.start_bugkill_flow(*request, tx);
            }
            DashboardAction::PushPullRequest(request) => {
                self.start_push_pr_flow(*request, tx);
            }
            DashboardAction::UpdateBranch { path, branch } => {
                self.start_update_branch_flow(path, branch, tx);
            }
            DashboardAction::ClosePullRequest(request) => {
                self.start_close_pr_flow(*request, tx);
            }
        }
    }

    fn handle_cache_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.cache.as_mut() {
            Some(cache) => cache.handle_key(key),
            None => return,
        };

        match action {
            CacheScreenAction::Continue => {}
            CacheScreenAction::Back => self.back_to_menu(),
            CacheScreenAction::Refresh => {
                if let Some(cache) = self.cache.as_mut() {
                    cache.start_loading();
                }
                kick_off_cache_load(self.git_root.clone(), tx.clone());
            }
            CacheScreenAction::DeleteEntry(relative_path) => {
                if let Some(cache) = self.cache.as_mut() {
                    cache.start_loading();
                }
                kick_off_cache_entry_delete(self.git_root.clone(), relative_path, tx.clone());
            }
        }
    }

    /// Mount the loading splash synchronously so the user gets an
    /// instant visual response, then kick off the background fetch +
    /// merge. On a clean merge the flow ends in
    /// `apply_update_branch_finished` with a toast; on conflicts it hands
    /// off to the opencode resolution screen (see that method).
    fn start_update_branch_flow(
        &mut self,
        worktree_path: String,
        branch: String,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        self.update_branch = Some(UpdateBranchScreen::new(worktree_path.clone(), branch));
        self.screen = Screen::UpdateBranch;
        kick_off_update_branch(self.current_dashboard_config(), worktree_path, tx.clone());
    }

    // ---- "Update all" batch (dashboard footer) ----------------------------
    //
    // Runs "Update branch (locally)" (Branches) or the full "Update"
    // (Pull Requests) across every displayed worktree, one at a time. Clean
    // updates advance immediately; merge conflicts hand off to opencode in
    // the embedded PTY, and `on_update_all_tick` auto-commits (Branches) or
    // commits + pushes (Pull Requests) once opencode exits, then advances —
    // all without user interaction, mirroring the single-worktree flows.

    fn start_update_all_branches(
        &mut self,
        targets: Vec<(String, String)>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        if targets.is_empty() {
            self.show_toast(ToastVariant::Info, "No worktrees to update.");
            return;
        }
        self.update_all = Some(UpdateAllRun::branches(targets));
        self.dispatch_next_update_all_branch(tx);
    }

    fn start_update_all_prs(
        &mut self,
        targets: Vec<UpdatePullRequestRequest>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        if targets.is_empty() {
            self.show_toast(ToastVariant::Info, "No pull requests need updating.");
            return;
        }
        self.update_all = Some(UpdateAllRun::pull_requests(targets));
        self.dispatch_next_update_all_pr(tx);
    }

    /// Pop the next Branches target and run its fetch + merge, or finish the
    /// batch when the queue is empty.
    fn dispatch_next_update_all_branch(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let next = self.update_all.as_mut().and_then(|run| {
            if run.branch_queue.is_empty() {
                return None;
            }
            let (path, branch) = run.branch_queue.remove(0);
            let done = run.total - run.branch_queue.len();
            Some((path, branch, done, run.total))
        });
        let Some((path, branch, done, total)) = next else {
            self.finish_update_all(tx);
            return;
        };
        let message = format!("Updating {branch} ({done}/{total})...");
        self.update_branch =
            Some(UpdateBranchScreen::new(path.clone(), branch).with_message(message));
        self.screen = Screen::UpdateBranch;
        kick_off_update_branch(self.current_dashboard_config(), path, tx.clone());
    }

    /// Pop the next Pull Requests target and run the full update pipeline
    /// (base ref resolved inside `kick_off_update_pull_request`), or finish
    /// the batch when the queue is empty. Skips the Confirm step — the batch
    /// runs unattended.
    fn dispatch_next_update_all_pr(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let next = self.update_all.as_mut().and_then(|run| {
            if run.pr_queue.is_empty() {
                return None;
            }
            let request = run.pr_queue.remove(0);
            let done = run.total - run.pr_queue.len();
            Some((request, done, run.total))
        });
        let Some((request, done, total)) = next else {
            self.finish_update_all(tx);
            return;
        };
        let number = request.number;
        let ai_update = self.current_dashboard_config().ai.update.clone();
        let mut screen = UpdatePullRequestScreen::new(request.clone(), ai_update);
        screen.start_updating();
        screen.set_phase_message(format!("Updating PR #{number} ({done}/{total})..."));
        self.update_pr = Some(screen);
        self.screen = Screen::UpdatePullRequest;
        kick_off_update_pull_request(
            self.git_root.clone(),
            self.current_dashboard_config(),
            request,
            tx.clone(),
        );
    }

    /// Branches-batch counterpart to `apply_update_branch_finished`: tally the
    /// outcome and advance, or hand conflicts to opencode (the tick driver
    /// finishes and advances that worktree).
    fn apply_update_all_branch_finished(
        &mut self,
        result: Result<UpdateBranchOutcome, String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let branch = self
            .update_branch
            .as_ref()
            .map(|s| s.branch().to_string())
            .unwrap_or_default();
        if let Ok(UpdateBranchOutcome::ConflictsHandedOffToUi {
            opencode_binary,
            opencode_args,
            cwd,
            model,
            base_ref,
            ..
        }) = result
        {
            self.start_local_conflict_resolution(
                branch,
                opencode_binary,
                opencode_args,
                cwd,
                model,
                base_ref,
            );
            return;
        }
        match result {
            Ok(UpdateBranchOutcome::AlreadyUpToDate { .. })
            | Ok(UpdateBranchOutcome::FastForwarded { .. })
            | Ok(UpdateBranchOutcome::Merged { .. }) => self.record_update_all_updated(),
            Ok(UpdateBranchOutcome::NoBaseRef)
            | Ok(UpdateBranchOutcome::WorkingTreeDirty { .. }) => self.record_update_all_skipped(),
            Ok(UpdateBranchOutcome::ConflictsRequireAi { conflicts }) => self
                .record_update_all_failure(format!(
                    "{branch}: {} conflict(s) need the `ai.update` model configured.",
                    conflicts.len()
                )),
            Ok(UpdateBranchOutcome::AiUnavailable { conflicts }) => {
                self.record_update_all_failure(format!(
                    "{branch}: {} conflict(s) but opencode is not on PATH.",
                    conflicts.len()
                ))
            }
            Ok(UpdateBranchOutcome::FetchFailed(message)) => {
                self.record_update_all_failure(format!("{branch}: git fetch failed: {message}"))
            }
            Ok(UpdateBranchOutcome::MergeFailed { message, .. }) => {
                self.record_update_all_failure(format!("{branch}: merge failed: {message}"))
            }
            // Handled by the early return above.
            Ok(UpdateBranchOutcome::ConflictsHandedOffToUi { .. }) => {}
            Err(message) => self.record_update_all_failure(format!("{branch}: {message}")),
        }
        self.dispatch_next_update_all_branch(tx);
    }

    /// Pull-Requests-batch counterpart: tally the terminal outcome and
    /// advance. Conflicts never reach here — `apply_update_pr_finished`
    /// spawns the opencode PTY and returns before delegating to this method.
    fn apply_update_all_pr_finished(
        &mut self,
        result: Result<UpdatePrSuccess, UpdatePrFailure>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        use crate::services::UpdatePullRequestOutcome;
        match result {
            Ok(UpdatePrSuccess {
                number, outcome, ..
            }) => match outcome {
                UpdatePullRequestOutcome::AlreadyUpToDate => self.record_update_all_skipped(),
                UpdatePullRequestOutcome::MergedCleanly | UpdatePullRequestOutcome::Pushed => {
                    self.record_update_all_updated()
                }
                UpdatePullRequestOutcome::MergedWithAiResolution => {
                    self.record_update_all_resolved()
                }
                UpdatePullRequestOutcome::ConflictsRequireAi { conflicts } => self
                    .record_update_all_failure(format!(
                        "PR #{number}: {} conflict(s) need the `ai.update` model configured.",
                        conflicts.len()
                    )),
                UpdatePullRequestOutcome::AiUnavailable { conflicts } => self
                    .record_update_all_failure(format!(
                        "PR #{number}: {} conflict(s) but opencode is not on PATH.",
                        conflicts.len()
                    )),
                UpdatePullRequestOutcome::FetchFailed(message) => self.record_update_all_failure(
                    format!("PR #{number}: git fetch failed: {message}"),
                ),
                UpdatePullRequestOutcome::MergeFailed(message) => {
                    self.record_update_all_failure(format!("PR #{number}: merge failed: {message}"))
                }
                UpdatePullRequestOutcome::PushFailed(message) => {
                    self.record_update_all_failure(format!("PR #{number}: push failed: {message}"))
                }
                // These come only from explicit Complete/Cancel actions the
                // batch never issues; treat defensively as failures.
                UpdatePullRequestOutcome::ConflictsHandedOffToUi { .. }
                | UpdatePullRequestOutcome::DiscardedAiMerge
                | UpdatePullRequestOutcome::AbortFailed(_) => self
                    .record_update_all_failure(format!("PR #{number}: update did not complete.")),
            },
            Err(UpdatePrFailure { number, message }) => {
                self.record_update_all_failure(format!("PR #{number}: {message}"))
            }
        }
        self.dispatch_next_update_all_pr(tx);
    }

    /// The Update-PR conflict-resolution turn watcher fired — mark the AI
    /// done automatically, exactly like the manual "Merge finalized?"
    /// confirm or a PTY exit would, so the Complete/Cancel buttons appear
    /// without the user needing to tell wisetree opencode is finished.
    /// Success and failure aren't distinguished here (same as PTY exit
    /// today): the human reviews the AI Activity panel and decides via
    /// Complete/Cancel either way.
    fn on_update_conflict_turn(&mut self, turn: OpencodeTurn) {
        if matches!(turn, OpencodeTurn::Working) {
            return;
        }
        self.update_conflict = None;
        if let Some(screen) = self.update_pr.as_mut() {
            screen.mark_ai_done();
        }
    }

    /// Called every tick while a batch is active. Drives the conflict-
    /// resolution screen to completion without user input: auto-commit once
    /// opencode exits, then advance once the commit shell exits. Step
    /// transitions (`Updating → CommitPush → torn down`) are the one-shot
    /// guards, since `tick_pty`/`poll_exited` keep reporting `true` every
    /// tick after a child exits.
    fn on_update_all_tick(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let Some(screen) = self.update_pr.as_ref() else {
            return;
        };
        let step = screen.step();
        let opencode_finished = screen.ai_active() && screen.ai_done();
        let commit_done = screen.commit_push_done();
        let commit_ok = screen.commit_push_succeeded();
        let branch = screen.request().branch.clone();

        if opencode_finished && matches!(step, UpdateStep::Updating) {
            // opencode resolved the conflicts → commit (local for Branches,
            // commit + push for Pull Requests). The step flips to CommitPush,
            // which prevents this branch from firing again.
            self.start_commit_after_ai();
            return;
        }
        if matches!(step, UpdateStep::CommitPush) && commit_done {
            if commit_ok {
                self.record_update_all_resolved();
            } else {
                self.record_update_all_failure(format!(
                    "{branch}: committing the resolved merge failed."
                ));
            }
            match self.update_all.as_ref().map(|run| run.kind) {
                Some(UpdateAllKind::Branches) => self.dispatch_next_update_all_branch(tx),
                Some(UpdateAllKind::PullRequests) => self.dispatch_next_update_all_pr(tx),
                None => {}
            }
        }
    }

    fn record_update_all_updated(&mut self) {
        if let Some(run) = self.update_all.as_mut() {
            run.updated += 1;
        }
    }

    fn record_update_all_resolved(&mut self) {
        if let Some(run) = self.update_all.as_mut() {
            run.resolved += 1;
        }
    }

    fn record_update_all_skipped(&mut self) {
        if let Some(run) = self.update_all.as_mut() {
            run.skipped += 1;
        }
    }

    fn record_update_all_failure(&mut self, message: String) {
        if let Some(run) = self.update_all.as_mut() {
            run.failed.push(message);
        }
    }

    /// Tear down the batch: drop the update screens, return to the dashboard,
    /// and surface a summary toast plus one warning per failed worktree
    /// (mirrors the bulk-delete summary).
    fn finish_update_all(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let Some(run) = self.update_all.take() else {
            return;
        };
        self.update_branch = None;
        self.update_pr = None;
        let noun = match run.kind {
            UpdateAllKind::Branches => "branches",
            UpdateAllKind::PullRequests => "pull requests",
        };
        self.enter_screen(Screen::Dashboard, tx);
        let variant = if run.failed.is_empty() {
            ToastVariant::Success
        } else {
            ToastVariant::Warning
        };
        self.show_toast(
            variant,
            format!(
                "Update all {noun}: {} updated, {} AI-resolved, {} skipped, {} failed.",
                run.updated,
                run.resolved,
                run.skipped,
                run.failed.len()
            ),
        );
        for failure in run.failed {
            self.show_toast(ToastVariant::Warning, failure);
        }
    }

    fn start_merge_pr_flow(
        &mut self,
        request: MergePullRequestRequest,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let number = request.number;
        let worktree_path = request.worktree_path.clone();
        self.merge_pr = Some(MergePullRequestScreen::new(request));
        self.screen = Screen::MergePullRequest;
        kick_off_fetch_pr_details(
            self.git_root.clone(),
            self.current_dashboard_config(),
            number,
            worktree_path,
            tx.clone(),
        );
    }

    fn start_update_pr_flow(
        &mut self,
        request: UpdatePullRequestRequest,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let worktree_path = request.worktree_path.clone();
        let number = request.number;
        let pr_base_ref = request.pr_base_ref.clone();
        let ai = self.current_dashboard_config().ai.update.clone();
        // Mount the screen with `base_ref = None` first so the confirm
        // panel renders immediately; the resolver runs in the background
        // and populates the field before the user can answer.
        self.update_pr = Some(UpdatePullRequestScreen::new(request, ai));
        self.screen = Screen::UpdatePullRequest;
        kick_off_resolve_base_ref(worktree_path, number, pr_base_ref, tx.clone());
    }

    fn start_explain_pr_flow(
        &mut self,
        request: ExplainPullRequestRequest,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let worktree_path = request.worktree_path.clone();
        let pr_base_ref = request.pr_base_ref.clone();
        let ai = self.current_dashboard_config().ai.explain.clone();
        // Mount with `base_ref = None` so the confirm panel renders straight
        // away; the resolver populates the field in the background.
        self.explain_pr = Some(ExplainPullRequestScreen::new(request, ai));
        self.screen = Screen::ExplainPullRequest;
        kick_off_resolve_explain_base_ref(worktree_path, pr_base_ref, tx.clone());
    }
    /// Mount the push-only confirmation screen. A push needs no base ref,
    /// so — unlike `start_update_pr_flow` — there's no resolver kick-off;
    /// the screen lands straight on the Confirm step. Confirmation routes
    /// to `kick_off_push_pull_request` (see `handle_update_pr_key`).
    fn start_push_pr_flow(
        &mut self,
        request: UpdatePullRequestRequest,
        _tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let ai = self.current_dashboard_config().ai.update.clone();
        self.update_pr = Some(UpdatePullRequestScreen::new_push(request, ai));
        self.screen = Screen::UpdatePullRequest;
    }

    fn start_close_pr_flow(
        &mut self,
        request: ClosePullRequestRequest,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        self.show_toast(
            ToastVariant::Info,
            format!("Closing Pull Request #{}…", request.number),
        );
        kick_off_close_pull_request(
            self.git_root.clone(),
            self.current_dashboard_config(),
            request.number,
            tx.clone(),
        );
    }

    fn current_dashboard_config(&self) -> DashboardConfig {
        self.current_config()
            .map(|cfg| cfg.dashboard.clone())
            .unwrap_or_default()
    }

    fn current_notifications_config(&self) -> NotificationsConfig {
        self.current_config()
            .map(|cfg| cfg.notifications.clone())
            .unwrap_or_default()
    }

    fn start_bulk_delete_flow(
        &mut self,
        status: BulkDeleteStatus,
        paths: Vec<String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        if paths.is_empty() {
            self.show_toast(
                ToastVariant::Info,
                format!("No worktrees with status '{}' to delete.", status.label()),
            );
            return;
        }
        self.pending_bulk_delete_paths = paths;
        self.enter_screen(Screen::Delete, tx);
    }

    fn handle_create_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.create.as_mut() {
            Some(create) => create.handle_key(key),
            None => return,
        };

        match action {
            CreateAction::Continue => {}
            CreateAction::Cancelled => self.back_to_menu(),
            CreateAction::Confirmed {
                directory_name,
                source_branch,
                new_branch,
            } => {
                if let Some(create) = self.create.as_mut() {
                    create.start_creating();
                }

                let options = WorktreeCreateOptions {
                    name: directory_name,
                    source_branch,
                    new_branch,
                    base_path: self.git_root.clone().unwrap_or_default(),
                };
                kick_off_create_worktree(self.git_root.clone(), options, tx.clone());
            }
            CreateAction::Done => {
                self.finish_create_success();
            }
        }
    }

    fn handle_delete_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.delete.as_mut() {
            Some(delete) => delete.handle_key(key),
            None => return,
        };

        match action {
            DeleteAction::Continue => {}
            DeleteAction::Cancelled => {
                self.cancel_delete_screen(tx);
            }
            DeleteAction::Confirmed { path, force } => {
                if let Some(delete) = self.delete.as_mut() {
                    delete.start_deleting();
                }
                kick_off_delete_worktree(self.git_root.clone(), path, force, tx.clone());
            }
            DeleteAction::BulkConfirmed { items } => {
                self.bulk_delete_queue = items;
                if let Some(delete) = self.delete.as_mut() {
                    delete.start_deleting();
                }
                self.dispatch_next_bulk_delete(tx);
            }
            DeleteAction::Done => self.leave_delete_screen(tx),
        }
    }

    fn dispatch_next_bulk_delete(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        if self.bulk_delete_queue.is_empty() {
            // Bulk run finished. Mirror the post-Create flow: surface a
            // success toast (plus any per-item warnings) and drop the
            // user back on the Dashboard rather than rendering a
            // dedicated success page.
            let summary = self.delete.as_mut().and_then(|d| d.take_bulk_summary());
            if let Some((message, warnings)) = summary {
                self.show_toast(ToastVariant::Success, message);
                for warning in warnings {
                    self.show_toast(ToastVariant::Warning, warning);
                }
            }
            // Bulk delete always originates from the Dashboard, so go
            // straight there. We can't rely on `leave_delete_screen`
            // here because `take_bulk_summary` already cleared the
            // bulk markers that `leave_delete_screen` inspects.
            self.pending_delete_path = None;
            self.pending_bulk_delete_paths.clear();
            self.maybe_redirect_git_root_to_mother();
            if self.quit_requested {
                return;
            }
            if self.git_root.is_some() {
                self.enter_screen(Screen::Dashboard, tx);
            } else {
                self.back_to_menu();
            }
            return;
        }
        let (path, force) = self.bulk_delete_queue.remove(0);
        kick_off_delete_worktree(self.git_root.clone(), path, force, tx.clone());
    }

    /// Exit the Delete screen back to wherever we came from. When the
    /// dashboard jumped us straight to a single-target or bulk confirm,
    /// return to the Dashboard rather than the main menu so the user
    /// lands where they started.
    fn leave_delete_screen(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let from_dashboard_single = self.pending_delete_path.take().is_some();
        let from_dashboard_bulk = self.delete.as_ref().map(|d| d.is_bulk()).unwrap_or(false)
            || !self.pending_bulk_delete_paths.is_empty();
        self.maybe_redirect_git_root_to_mother();
        if self.quit_requested {
            return;
        }
        if (from_dashboard_single || from_dashboard_bulk) && self.git_root.is_some() {
            self.enter_screen(Screen::Dashboard, tx);
        } else {
            self.back_to_menu();
        }
    }

    /// If the current `git_root` directory no longer exists on disk (e.g. the
    /// user just deleted the worktree they launched wisetree from), redirect
    /// to the main/mother worktree: update `git_root`, change this process's
    /// cwd, and — in wrapper mode — quit so the shell lands in the mother path.
    fn maybe_redirect_git_root_to_mother(&mut self) {
        let needs_redirect = self
            .git_root
            .as_deref()
            .map(|p| !std::path::Path::new(p).exists())
            .unwrap_or(false);
        if !needs_redirect {
            return;
        }
        let Some(main_path) = self.dashboard.as_ref().and_then(|d| d.main_worktree_path()) else {
            return;
        };
        self.git_root = Some(main_path.clone());
        // Update the process cwd so git commands executed from here resolve
        // correctly even if the caller stays in the TUI (non-wrapper mode).
        let _ = std::env::set_current_dir(&main_path);
        if self.is_from_wrapper {
            self.selected_path = Some(main_path);
            self.quit_requested = true;
        }
    }

    /// Cancel the Delete screen and return to the preserved dashboard. Unlike
    /// `leave_delete_screen` (which re-creates the dashboard from scratch after
    /// a completed deletion), this path keeps the existing `self.dashboard`
    /// instance so the user's row selection, scroll position, and any other
    /// in-flight state survive the round-trip.
    fn cancel_delete_screen(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        let from_dashboard_single = self.pending_delete_path.take().is_some();
        let from_dashboard_bulk = self.delete.as_ref().map(|d| d.is_bulk()).unwrap_or(false)
            || !self.pending_bulk_delete_paths.is_empty();

        if (from_dashboard_single || from_dashboard_bulk) && self.git_root.is_some() {
            if self.dashboard.is_some() {
                // A dashboard instance was preserved when we entered the delete
                // screen. Restore it directly to keep selection state intact.
                self.delete = None;
                self.pending_bulk_delete_paths.clear();
                self.bulk_delete_queue.clear();
                self.screen = Screen::Dashboard;
                // The watch was dropped when we entered the delete screen.
                // Restore it so the dashboard keeps receiving live updates.
                if self.dashboard_watch.is_none() {
                    if let Some(git_root) = self.git_root.as_ref().map(std::path::PathBuf::from) {
                        let config = self
                            .current_config()
                            .map(|cfg| cfg.dashboard.clone())
                            .unwrap_or_default();
                        let service = DashboardService::new(git_root, config);
                        self.dashboard_watch = Some(service.watch());
                        self.dashboard_notification_snapshot = None;
                    }
                }
            } else {
                // No preserved dashboard (e.g. delete was opened from the menu
                // rather than the Backspace shortcut). Fall back to a fresh one.
                self.enter_screen(Screen::Dashboard, tx);
            }
        } else {
            self.back_to_menu();
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.settings.as_mut() {
            Some(settings) => settings.handle_key(key),
            None => return,
        };

        match action {
            SettingsAction::Continue => {}
            SettingsAction::Back => self.back_to_menu(),
            SettingsAction::CopySettingsFilePath => {
                let path = self.settings_edit_file_path().display().to_string();
                kick_off_clipboard_copy(path, SETTINGS_PATH_COPIED_MESSAGE.to_string(), tx.clone());
            }
            SettingsAction::CheckUpdates => {
                if let Some(settings) = self.settings.as_mut() {
                    settings.start_checking_updates();
                }
                kick_off_update_check(tx.clone());
            }
            SettingsAction::UpgradeSource(source) => {
                if let Some(settings) = self.settings.as_mut() {
                    settings.start_upgrade(source);
                }
                kick_off_upgrade(source, tx.clone());
            }
            SettingsAction::SetDeleteBranchWithWorktree(enabled) => {
                if let Err(err) = self.save_delete_branch_with_worktree(enabled) {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to update configuration: {err}"));
                    }
                }
            }
            SettingsAction::Reset => {
                if let Err(err) = self.reset_settings_config() {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to reset configuration: {err}"));
                    }
                }
            }
            SettingsAction::SaveCopyPatterns(patterns) => {
                if let Err(err) = self.save_copy_patterns(patterns) {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to save copy patterns: {err}"));
                    }
                }
            }
            SettingsAction::SaveIgnorePatterns(patterns) => {
                if let Err(err) = self.save_ignore_patterns(patterns) {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to save ignore patterns: {err}"));
                    }
                }
            }
            SettingsAction::SaveLinkPatterns(patterns) => {
                if let Err(err) = self.save_link_patterns(patterns) {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to save link patterns: {err}"));
                    }
                }
            }
            SettingsAction::CopySettings(direction) => {
                if let Err(err) = self.copy_settings(direction) {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to copy settings: {err}"));
                    }
                }
            }
            SettingsAction::SavePostCreateCommands(commands) => {
                if let Err(err) = self.save_post_create_commands(commands) {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to save post-create commands: {err}"));
                    }
                }
            }
            SettingsAction::SaveTerminalCommand(command) => {
                if let Err(err) = self.save_terminal_command(command) {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to save terminal command: {err}"));
                    }
                }
            }
            SettingsAction::SavePathTemplate(template) => {
                if let Err(err) = self.save_path_template(template) {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to save path template: {err}"));
                    }
                }
            }
            SettingsAction::SaveLinkStrategy(strategy) => {
                if let Err(err) = self.save_link_strategy(strategy) {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to save link strategy: {err}"));
                    }
                }
            }
            SettingsAction::SaveLinkCacheDir(cache_dir) => {
                if let Err(err) = self.save_link_cache_dir(cache_dir) {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to save link cache dir: {err}"));
                    }
                }
            }
            SettingsAction::SaveDashboard(dashboard) => {
                if let Err(err) = self.save_dashboard(*dashboard) {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to save dashboard settings: {err}"));
                    }
                }
            }
            SettingsAction::SaveNotifications(notifications) => {
                if let Err(err) = self.save_notifications(notifications) {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to save notification settings: {err}"));
                    }
                }
            }
            SettingsAction::OpenAiModelPicker { model, variant } => {
                self.open_ai_model_picker(model, variant, tx);
            }
            SettingsAction::FetchFreeModels => {
                kick_off_fetch_free_opencode_models(tx.clone());
                kick_off_fetch_ai_model_variants(tx.clone());
            }
            SettingsAction::OpenSetupProject => {
                self.enter_screen(Screen::SetupProject, tx);
            }
        }
    }

    fn handle_setup_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.setup.as_mut() {
            Some(setup) => setup.handle_key(key),
            None => return,
        };

        match action {
            SetupAction::Continue => {}
            SetupAction::Cancelled => self.back_to_menu(),
            SetupAction::Confirmed { shell } => {
                if let Some(setup) = self.setup.as_mut() {
                    setup.start_installing();
                }
                kick_off_setup_install(shell, tx.clone());
            }
            SetupAction::Done => self.back_to_menu(),
        }
    }

    fn handle_setup_project_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        let action = match self.setup_project.as_mut() {
            Some(screen) => screen.handle_key(key),
            None => return,
        };

        match action {
            SetupProjectAction::Continue => {}
            SetupProjectAction::Cancelled => self.back_to_menu(),
            SetupProjectAction::DiscoverWise => self.start_wise_preset_discovery(tx),
            SetupProjectAction::Apply(preset) => self.apply_setup_project_preset(preset),
        }
    }

    fn start_wise_preset_discovery(&mut self, tx: &mpsc::UnboundedSender<AppEvent>) {
        if self.git_root.is_none() {
            if let Some(screen) = self.setup_project.as_mut() {
                screen.reset_after_wise_discovery_failure();
            }
            self.show_toast(
                ToastVariant::Error,
                "No git repository in scope for Wise Preset discovery.",
            );
            return;
        }

        self.show_toast(
            ToastVariant::Info,
            "Wise Preset is scanning the repository...",
        );
        kick_off_wise_preset_discovery(self.git_root.clone(), tx.clone());
    }

    fn apply_setup_project_preset(&mut self, preset: SetupProjectPresetValues) {
        let applied_label = if preset.label == "Wise Preset" {
            preset.label.clone()
        } else {
            format!("{} preset", preset.label)
        };

        let local_path = match self.local_config_path() {
            Some(path) => path,
            None => {
                self.show_toast(
                    ToastVariant::Error,
                    "No git repository in scope — cannot write .wisetree.json.",
                );
                return;
            }
        };

        let mut config = self.current_config().cloned().unwrap_or_default();
        config.worktree_copy_patterns = preset.copy_patterns;
        config.worktree_copy_ignores = preset.copy_ignores;
        config.worktree_link_patterns = preset.link_patterns;
        config.worktree_link_strategy = if config.worktree_link_patterns.is_empty() {
            LinkStrategy::CreateEmpty
        } else {
            LinkStrategy::SeedFromSource
        };
        config.post_create_cmd = preset.post_create_cmd;

        let mut writer = ConfigService::new();
        if let Err(err) = writer.save(&config, Some(&local_path)) {
            self.show_toast(
                ToastVariant::Error,
                format!("Failed to write .wisetree.json: {err}"),
            );
            return;
        }

        if let Some(service) = self.worktree_service.as_mut() {
            let _ = service.config_service_mut().load(local_path.parent());
        }

        self.show_toast(
            ToastVariant::Success,
            format!("Applied {applied_label} to .wisetree.json"),
        );
        self.back_to_menu();
    }

    fn apply_wise_preset_discovery(&mut self, result: Result<WisePresetDiscovery, String>) {
        let Some(screen) = self.setup_project.as_mut() else {
            return;
        };

        match result {
            Ok(discovery) => {
                let summary = summarize_wise_preset_matches(&discovery);
                let used_generic_fallback = discovery.used_generic_fallback();
                screen.complete_wise_discovery(discovery);
                if used_generic_fallback {
                    self.show_toast(
                        ToastVariant::Warning,
                        "Wise Preset found no specific frameworks. Using Generic values.",
                    );
                } else {
                    self.show_toast(
                        ToastVariant::Success,
                        format!("Wise Preset found {summary}. Review and apply."),
                    );
                }
            }
            Err(message) => {
                screen.reset_after_wise_discovery_failure();
                self.show_toast(
                    ToastVariant::Error,
                    format!("Wise Preset discovery failed: {message}"),
                );
            }
        }
    }

    fn handle_app_event(&mut self, event: AppEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        match event {
            AppEvent::Initialized(outcome) => self.apply_init_outcome(*outcome, tx),
            AppEvent::CacheLoaded(result) | AppEvent::CacheEntryDeleted(result) => {
                if let Some(cache) = self.cache.as_mut() {
                    match result {
                        Ok(overview) => cache.set_overview(overview),
                        Err(message) => cache.set_error(message),
                    }
                }
            }
            AppEvent::CreateBranchesLoaded(result) => {
                if let Some(create) = self.create.as_mut() {
                    match result {
                        Ok(branches) => create.set_branches(branches),
                        Err(message) => create.set_branches_error(message),
                    }
                }
            }
            AppEvent::CreateFinished(result) => {
                if let Some(create) = self.create.as_mut() {
                    match result {
                        Ok(path) => {
                            let rows = create_summary_rows(&path);
                            create.set_created_worktree_path(path.worktree_path.clone());
                            create.mark_complete(rows);
                        }
                        Err(message) => create.set_error(message),
                    }
                }
            }
            AppEvent::CreateActivity { text, kind } => {
                if let Some(create) = self.create.as_mut() {
                    create.append_terminal_line(text, kind);
                }
            }
            AppEvent::DeleteLoaded(result) => {
                if let Some(delete) = self.delete.as_mut() {
                    match result {
                        Ok(worktrees) => {
                            delete.set_worktrees(worktrees);
                            if !self.pending_bulk_delete_paths.is_empty() {
                                let paths = std::mem::take(&mut self.pending_bulk_delete_paths);
                                delete.jump_to_bulk_confirm(paths);
                            } else if let Some(path) = self.pending_delete_path.as_deref() {
                                delete.jump_to_confirm_path(path);
                            }
                        }
                        Err(message) => delete.set_error(message),
                    }
                }
            }
            AppEvent::DeleteFinished(result) => {
                let in_bulk = self.delete.as_ref().map(|d| d.is_bulk()).unwrap_or(false);
                match result {
                    Ok(outcome) => {
                        if in_bulk {
                            // Defer per-item branch warnings to the end of
                            // the bulk run so they're surfaced together
                            // with the summary toast (otherwise a long run
                            // would flash many 5-second warning toasts
                            // back-to-back, hiding earlier ones).
                            if let Some(delete) = self.delete.as_mut() {
                                delete.bulk_record_progress(outcome.branch_delete_error.clone());
                            }
                            self.dispatch_next_bulk_delete(tx);
                        } else if let Some(warning) = outcome.branch_delete_error.clone() {
                            let screen_outcome = screen_delete_outcome(outcome);
                            if let Some(delete) = self.delete.as_mut() {
                                delete.mark_complete(screen_outcome);
                            }
                            self.show_toast(ToastVariant::Warning, warning);
                        } else {
                            let screen_outcome = screen_delete_outcome(outcome);
                            let success_msg = self
                                .delete
                                .as_ref()
                                .map(|d| d.success_message_for(&screen_outcome))
                                .unwrap_or_else(|| DELETE_SUCCESS.to_string());
                            self.show_toast(ToastVariant::Success, success_msg);
                            self.leave_delete_screen(tx);
                        }
                    }
                    Err(message) => {
                        // Abort the remaining bulk run on the first failure
                        // and surface the error.
                        self.bulk_delete_queue.clear();
                        if let Some(delete) = self.delete.as_mut() {
                            delete.set_error(message);
                        }
                    }
                }
            }
            AppEvent::SettingsUpdateChecked(result) => {
                if let Some(settings) = self.settings.as_mut() {
                    settings.set_update_result(result);
                }
            }
            AppEvent::SettingsUpgradeFinished { source, result } => {
                let outcome = match result {
                    Ok(message) => UpgradeOutcome {
                        source,
                        success: true,
                        message,
                    },
                    Err(message) => UpgradeOutcome {
                        source,
                        success: false,
                        message,
                    },
                };
                let variant = if outcome.success {
                    ToastVariant::Success
                } else {
                    ToastVariant::Error
                };
                let toast_msg = format!("{}: {}", source.label(), outcome.message);
                if let Some(settings) = self.settings.as_mut() {
                    settings.set_upgrade_outcome(outcome);
                }
                self.show_toast(variant, toast_msg);
            }
            AppEvent::SetupInstalled(result) => {
                if let Some(setup) = self.setup.as_mut() {
                    match result {
                        Ok(status) => {
                            self.shell_integration_status = Some(status);
                            self.menu = None;
                            setup.mark_complete();
                        }
                        Err(message) => setup.set_error(message),
                    }
                }
            }
            AppEvent::ClipboardCopyFinished {
                success_message,
                error,
            } => match error {
                None => self.show_toast(ToastVariant::Info, success_message),
                Some(err) => {
                    self.show_toast(ToastVariant::Error, format!("Clipboard copy failed: {err}"))
                }
            },
            AppEvent::WisePresetDiscovered(result) => self.apply_wise_preset_discovery(result),
            AppEvent::MergePrDetailsLoaded(result) => self.apply_merge_pr_details(result, tx),
            AppEvent::MergePrFinished(result) => self.apply_merge_pr_finished(result, tx),
            AppEvent::ClosePrFinished(result) => self.apply_close_pr_finished(result),
            AppEvent::UpdatePrBaseRefResolved { number, base_ref } => {
                self.apply_update_pr_base_ref(number, base_ref);
            }
            AppEvent::UpdatePrProgress { number, progress } => {
                self.apply_update_pr_progress(number, progress);
            }
            AppEvent::UpdatePrFinished(result) => self.apply_update_pr_finished(result, tx),
            AppEvent::UpdateBranchFinished(result) => self.apply_update_branch_finished(result, tx),
            AppEvent::ExplainPrBaseRefResolved { base_ref } => {
                self.apply_explain_pr_base_ref(base_ref);
            }
            AppEvent::ExplainPrPrepared(result) => self.apply_explain_pr_prepared(result, tx),
            AppEvent::ExplainPrSubmitted(result) => self.apply_explain_pr_submitted(result, tx),
            AppEvent::ExplainPrActivity { text, kind } => {
                if let Some(screen) = self.explain_pr.as_mut() {
                    screen.append_terminal_line(text, kind);
                }
            }
            AppEvent::FixPrPrepared(result) => self.apply_fix_pr_prepared(result, tx),
            AppEvent::FixPrPlanned {
                index,
                is_replan,
                result,
            } => self.apply_fix_pr_planned(index, is_replan, result, tx),
            AppEvent::FixPrReplied { index, result } => {
                self.apply_fix_pr_replied(index, result, tx)
            }
            AppEvent::FixPrApplyReady { index, result } => {
                self.apply_fix_pr_apply_ready(index, result, tx)
            }
            AppEvent::FixPrCommitted { index, result } => {
                self.apply_fix_pr_committed(index, result, tx)
            }
            AppEvent::ReviewPrPrepared(result) => self.apply_review_pr_prepared(result, tx),
            AppEvent::ReviewPrScanned {
                file_index,
                retry,
                result,
                telemetry,
                raw_output,
            } => self.apply_review_pr_scanned(file_index, retry, result, telemetry, raw_output, tx),
            AppEvent::ReviewPrRevised {
                index,
                mode,
                feedback,
                result,
                telemetry,
            } => self.apply_review_pr_revised(index, mode, feedback, result, telemetry, tx),
            AppEvent::ReviewPrVerified {
                index,
                result,
                telemetry,
            } => self.apply_review_pr_verified(index, result, telemetry),
            AppEvent::ReviewPrGapAudited { result, telemetry } => {
                self.apply_review_pr_gap_audited(result, telemetry, tx)
            }
            AppEvent::ReviewPrPosted { index, result } => {
                self.apply_review_pr_posted(index, result, tx)
            }
            AppEvent::ReviewPrSummarySubmitted {
                request_changes,
                result,
            } => self.apply_review_pr_summary_submitted(request_changes, result),
            AppEvent::BugkillPrepared(result) => self.apply_bugkill_prepared(result, tx),
            AppEvent::BugkillDiscarded(result) => self.apply_bugkill_discarded(result, tx),
            AppEvent::BugkillInvestigateReady { corrective, result } => {
                self.apply_bugkill_investigate_ready(corrective, result)
            }
            AppEvent::BugkillFixReady { row_index, result } => {
                self.apply_bugkill_fix_ready(row_index, result)
            }
            AppEvent::BugkillCommitted(result) => self.apply_bugkill_committed(result, tx),
            AppEvent::BugkillAborted(result) => self.apply_bugkill_aborted(result),
            AppEvent::BugkillJudged { user_text, result } => {
                self.apply_bugkill_judged(user_text, result, tx)
            }
            AppEvent::BugkillRolledBack(result) => self.apply_bugkill_rolled_back(result, tx),
            AppEvent::BugkillFileWriteFailed(err) => self.show_toast(
                ToastVariant::Warning,
                format!("Could not write BUG_INVESTIGATION.md: {err}"),
            ),
            AppEvent::FixPrPushed(result) => {
                if let Some(screen) = self.fix_pr.as_mut() {
                    screen.enter_done(result);
                }
            }
            AppEvent::AiModelsFetched(result) => {
                // The fetch is best-effort: by the time it returns the user may
                // have already closed the picker. Silently drop the result in
                // that case — there's nothing to update.
                if let Some(picker) = self.ai_model_picker.as_mut() {
                    match result {
                        Ok(models) => picker.set_models(models),
                        Err(message) => picker.set_error(message),
                    }
                }
            }
            AppEvent::FreeOpencodeModelsFetched(result) => {
                // Same best-effort posture as the picker fetch: by the time
                // this lands the user may have already left the Dashboard
                // editor, so we silently drop the result if there's no
                // Settings screen to update.
                if let Some(settings) = self.settings.as_mut() {
                    match result {
                        Ok(models) => settings.set_free_models(models),
                        Err(message) => settings.set_free_models_error(message),
                    }
                }
            }
            AppEvent::AiModelVariantsFetched(result) => {
                // Best-effort, like the free-model fetch: drop the result if
                // the user has already left the Settings screen, and ignore
                // errors (the cycle falls back to the generic ladder).
                if let (Some(settings), Ok(variants)) = (self.settings.as_mut(), result) {
                    settings.set_ai_model_variants(variants);
                }
            }
            AppEvent::ShellIntegrationDetected(status) => {
                self.shell_integration_status = Some(status);
            }
        }
    }

    fn apply_update_branch_finished(
        &mut self,
        result: Result<UpdateBranchOutcome, String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        // During an "Update all → Branches" batch this event belongs to the
        // batch driver, which tallies + advances instead of toasting and
        // returning to the dashboard.
        if matches!(
            self.update_all.as_ref().map(|run| run.kind),
            Some(UpdateAllKind::Branches)
        ) {
            self.apply_update_all_branch_finished(result, tx);
            return;
        }
        // Capture the branch off the splash before we replace it — the
        // conflict-resolution screen uses it for its synthetic request.
        let branch = self
            .update_branch
            .as_ref()
            .map(|s| s.branch().to_string())
            .unwrap_or_default();
        match result {
            // Conflicts with AI available: don't toast — hand off to the
            // opencode resolution screen. The merge is left mid-flight on
            // disk (conflict markers in the index); the screen owns the
            // PTY from here and commits the result locally (no push).
            Ok(UpdateBranchOutcome::ConflictsHandedOffToUi {
                opencode_binary,
                opencode_args,
                cwd,
                model,
                base_ref,
                ..
            }) => {
                self.start_local_conflict_resolution(
                    branch,
                    opencode_binary,
                    opencode_args,
                    cwd,
                    model,
                    base_ref,
                );
            }
            // Every other outcome drops the loading splash and routes back
            // to the dashboard before toasting — the user must land on the
            // screen where the toast appears, otherwise the result would
            // flash on the splash for one frame and vanish.
            other => {
                self.update_branch = None;
                if matches!(self.screen, Screen::UpdateBranch) {
                    self.enter_screen(Screen::Dashboard, tx);
                }
                self.show_update_branch_toast(other);
            }
        }
    }

    /// Mount the opencode conflict-resolution screen for the "Update branch
    /// (locally)" flow. Reuses `UpdatePullRequestScreen` in `local_only`
    /// mode: it streams opencode in the embedded PTY, then commits the
    /// merge locally on **Complete** (no push) and shows the ✅ done page.
    fn start_local_conflict_resolution(
        &mut self,
        branch: String,
        opencode_binary: PathBuf,
        opencode_args: Vec<String>,
        cwd: PathBuf,
        model: String,
        base_ref: String,
    ) {
        let request = UpdatePullRequestRequest {
            number: 0,
            title: String::new(),
            url: String::new(),
            branch,
            worktree_path: cwd.to_string_lossy().to_string(),
            ahead: 0,
            behind: 0,
            base_ref: Some(base_ref),
            pr_base_ref: None,
            // The local-conflict tail skips the confirm screen, so it cannot
            // expose the autonomous toggle; keep the default behavior.
            autonomous: true,
        };
        let ai = self.current_dashboard_config().ai.update.clone();
        let mut screen = UpdatePullRequestScreen::new_local_conflict(request, ai);
        screen.set_phase_message(format!("{model} is resolving conflicts..."));
        // Launch opencode through the user's login shell so it runs with the
        // same profile-sourced environment as a freshly opened terminal
        // (matching the Update Pull Request flow).
        let (shell, wrapped_args) = login_shell_command(&opencode_binary, &opencode_args);
        // Watcher must exist before the spawn so its start timestamp
        // precedes the session row opencode creates.
        self.update_conflict = Some(OpencodeTurnWatcher::new(&cwd));
        screen.spawn_opencode_pty(shell, wrapped_args, cwd, Vec::new());
        self.update_branch = None;
        self.update_pr = Some(screen);
        self.screen = Screen::UpdatePullRequest;
    }

    fn show_update_branch_toast(&mut self, result: Result<UpdateBranchOutcome, String>) {
        match result {
            Ok(UpdateBranchOutcome::AlreadyUpToDate { base_ref }) => self.show_toast(
                ToastVariant::Info,
                format!("Already up to date with {base_ref}."),
            ),
            Ok(UpdateBranchOutcome::FastForwarded { base_ref, summary }) => self.show_toast(
                ToastVariant::Info,
                format!("Fast-forwarded to {base_ref} ({summary})."),
            ),
            Ok(UpdateBranchOutcome::Merged { base_ref, summary }) => self.show_toast(
                ToastVariant::Info,
                format!("Merged {base_ref} ({summary})."),
            ),
            Ok(UpdateBranchOutcome::NoBaseRef) => self.show_toast(
                ToastVariant::Warning,
                "No upstream/main, upstream/master, origin/main, or origin/master ref \
                 was reachable to update from."
                    .to_string(),
            ),
            Ok(UpdateBranchOutcome::FetchFailed(message)) => {
                self.show_toast(ToastVariant::Error, format!("git fetch failed: {message}"))
            }
            Ok(UpdateBranchOutcome::MergeFailed { base_ref, message }) => self.show_toast(
                ToastVariant::Error,
                format!("git merge {base_ref} failed: {message}"),
            ),
            Ok(UpdateBranchOutcome::WorkingTreeDirty { files }) => self.show_toast(
                ToastVariant::Warning,
                format!(
                    "{} uncommitted change(s) in the worktree — commit or stash them \
                     before updating.",
                    files.len()
                ),
            ),
            Ok(UpdateBranchOutcome::ConflictsRequireAi { .. }) => self.show_toast(
                ToastVariant::Warning,
                "Conflicts found, please resolve them locally or set the `ai.update` \
                 model so we can solve conflicts + merge via AI."
                    .to_string(),
            ),
            Ok(UpdateBranchOutcome::AiUnavailable { conflicts }) => self.show_toast(
                ToastVariant::Error,
                format!(
                    "Merge has {} conflicted file(s). `opencode` CLI is not on PATH — \
                     install it from https://opencode.ai then retry.",
                    conflicts.len()
                ),
            ),
            // Handled by `apply_update_branch_finished` before reaching the
            // toast path (it mounts the resolution screen instead).
            Ok(UpdateBranchOutcome::ConflictsHandedOffToUi { .. }) => {}
            Err(message) => self.show_toast(
                ToastVariant::Error,
                format!("Update branch failed: {message}"),
            ),
        }
    }

    /// Translate a single `UpdateProgress` event into UI state changes:
    /// phase transitions become toasts + an updated spinner label, AI
    /// output lines append to the streaming activity panel.
    fn apply_update_pr_progress(&mut self, number: u64, progress: UpdateProgress) {
        // If the user already left the screen (Esc during the run), drop
        // late events silently — there's nothing to update and toasting
        // out-of-flow phases would surprise them.
        let stale = self
            .update_pr
            .as_ref()
            .map(|s| s.request().number != number)
            .unwrap_or(true);
        if stale {
            return;
        }
        match progress {
            UpdateProgress::Phase(phase) => self.apply_update_pr_phase(number, phase),
            UpdateProgress::AiOutput(line) => {
                if let Some(screen) = self.update_pr.as_mut() {
                    screen.append_ai_line(line);
                }
            }
        }
    }

    fn apply_update_pr_phase(&mut self, number: u64, phase: UpdatePhase) {
        match phase {
            UpdatePhase::Fetching => {
                self.set_update_pr_phase_label("Fetching latest from remotes...");
            }
            UpdatePhase::AlreadyUpToDate => {
                self.show_toast(
                    ToastVariant::Info,
                    format!("Pull Request #{number} is already up to date — no action needed."),
                );
            }
            UpdatePhase::Merging => {
                self.set_update_pr_phase_label("Merging base ref into branch...");
            }
            UpdatePhase::NoConflicts => {
                self.show_toast(
                    ToastVariant::Success,
                    format!("No conflicts in PR #{number} — merging ahead and pushing to origin."),
                );
                self.set_update_pr_phase_label("Pushing merge to origin...");
            }
            UpdatePhase::ConflictsDetected { count } => {
                self.show_toast(
                    ToastVariant::Warning,
                    format!("PR #{number}: {count} conflicted file(s) — handing off to opencode."),
                );
                if let Some(screen) = self.update_pr.as_mut() {
                    screen.mark_ai_active();
                }
            }
            UpdatePhase::AiResolving { model } => {
                self.set_update_pr_phase_label(format!("{model} is resolving conflicts..."));
            }
            UpdatePhase::Committing => {
                self.set_update_pr_phase_label("Staging resolved files and committing...");
            }
            UpdatePhase::Pushing => {
                self.set_update_pr_phase_label("Pushing merge to origin...");
            }
        }
    }

    fn set_update_pr_phase_label(&mut self, label: impl Into<String>) {
        if let Some(screen) = self.update_pr.as_mut() {
            screen.set_phase_message(label);
        }
    }

    fn apply_merge_pr_details(
        &mut self,
        result: Result<MergePrDetailsPayload, String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        // If the user already left the merge screen (Esc during load) we
        // drop the result silently — there's no screen left to update and
        // toasting would surprise the user.
        let Some(screen) = self.merge_pr.as_mut() else {
            return;
        };
        match result {
            Ok(payload) => {
                screen.override_title(payload.title);
                screen.set_body(payload.body);
                screen.set_unpushed_commits(payload.unpushed_commits);
            }
            Err(message) => {
                self.show_toast(
                    ToastVariant::Error,
                    format!("Failed to load pull request details: {message}"),
                );
                self.merge_pr = None;
                self.enter_screen(Screen::Dashboard, tx);
            }
        }
    }

    fn apply_update_pr_base_ref(&mut self, number: u64, base_ref: Option<String>) {
        let Some(screen) = self.update_pr.as_mut() else {
            return;
        };
        if screen.request().number != number {
            return;
        }
        match base_ref {
            Some(base_ref) => screen.set_base_ref(base_ref),
            None => screen.set_error(
                "No base ref reachable (looked for upstream/main, upstream/master, \
                 origin/main, origin/master)."
                    .to_string(),
            ),
        }
    }

    fn apply_update_pr_finished(
        &mut self,
        result: Result<UpdatePrSuccess, UpdatePrFailure>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        use crate::services::UpdatePullRequestOutcome;
        // The "Update branch (locally)" flow reuses this screen in
        // `local_only` mode (no PR); its toasts must not mention a PR.
        let local_only = self
            .update_pr
            .as_ref()
            .map(|s| s.local_only())
            .unwrap_or(false);
        // `ConflictsHandedOffToUi` does NOT close the screen — the
        // service paused mid-flight (conflicts in the index, opencode
        // not yet invoked). We spawn opencode inside the screen's
        // embedded PTY here; the screen ticks the PTY each frame and
        // flips into the Complete/Cancel decision step once the child
        // exits. All other variants are terminal.
        if let Ok(UpdatePrSuccess {
            outcome:
                UpdatePullRequestOutcome::ConflictsHandedOffToUi {
                    opencode_binary,
                    opencode_args,
                    cwd,
                    ..
                },
            ..
        }) = &result
        {
            if let Some(screen) = self.update_pr.as_mut() {
                // Launch opencode *through* the user's login shell so it runs
                // with the same profile-sourced environment as a freshly
                // opened terminal (matching the recovery shell below).
                let (shell, wrapped_args) = login_shell_command(opencode_binary, opencode_args);
                // Watcher must exist before the spawn so its start timestamp
                // precedes the session row opencode creates.
                self.update_conflict = Some(OpencodeTurnWatcher::new(cwd));
                screen.spawn_opencode_pty(shell, wrapped_args, cwd.clone(), Vec::new());
                return;
            }
        }
        // During an "Update all → Pull Requests" batch, every non-conflict
        // outcome (including a failed push) is tallied and advances the queue
        // rather than toasting or opening the interactive recovery panel.
        if matches!(
            self.update_all.as_ref().map(|run| run.kind),
            Some(UpdateAllKind::PullRequests)
        ) {
            self.apply_update_all_pr_finished(result, tx);
            return;
        }
        // A failed `git push` (clean-merge push, AI commit+push, or the
        // dedicated Push action) does NOT dead-end on a toast. We hand off
        // to the interactive Terminal Activity recovery panel — a real
        // shell rooted at the worktree — so the user can diagnose and fix
        // it, then Accept (re-push) or Discard. Only falls through to the
        // toast below if the screen was already torn down.
        if let Ok(UpdatePrSuccess {
            outcome: UpdatePullRequestOutcome::PushFailed(err),
            ..
        }) = &result
        {
            if let Some(screen) = self.update_pr.as_mut() {
                let (shell, args) = login_shell();
                let cwd = PathBuf::from(&screen.request().worktree_path);
                screen.start_terminal_recovery(shell, args, cwd, err.clone());
                return;
            }
        }
        match result {
            Ok(UpdatePrSuccess {
                number,
                base_ref,
                outcome,
            }) => match outcome {
                UpdatePullRequestOutcome::AlreadyUpToDate => {
                    self.show_toast(
                        ToastVariant::Info,
                        format!("Pull Request #{number} is already up to date with `{base_ref}`."),
                    );
                }
                UpdatePullRequestOutcome::MergedCleanly => {
                    self.show_toast(
                        ToastVariant::Success,
                        format!("Pull Request #{number} updated with `{base_ref}` and pushed."),
                    );
                }
                UpdatePullRequestOutcome::Pushed => {
                    self.show_toast(
                        ToastVariant::Success,
                        format!("Pull Request #{number} pushed to origin."),
                    );
                }
                UpdatePullRequestOutcome::MergedWithAiResolution => {
                    self.show_toast(
                        ToastVariant::Success,
                        format!("Pull Request #{number} updated (opencode-resolved) and pushed."),
                    );
                }
                UpdatePullRequestOutcome::ConflictsHandedOffToUi { .. } => {
                    // Handled by the early-return branch above; this arm
                    // only fires if `update_pr` was already torn down.
                }
                UpdatePullRequestOutcome::DiscardedAiMerge => {
                    let message = if local_only {
                        "Discarded the update — branch is back where it was \
                         before the merge."
                            .to_string()
                    } else {
                        format!(
                            "Discarded AI merge for PR #{number}. \
                             Branch is back where it was before the update."
                        )
                    };
                    self.show_toast(ToastVariant::Warning, message);
                }
                UpdatePullRequestOutcome::ConflictsRequireAi { .. } => {
                    self.show_toast(
                        ToastVariant::Warning,
                        "Conflicts found, please resolve them locally or set the `ai.update` \
                         model so we can solve conflicts + merge via AI."
                            .to_string(),
                    );
                }
                UpdatePullRequestOutcome::AiUnavailable { conflicts } => {
                    let count = conflicts.len();
                    self.show_toast(
                        ToastVariant::Error,
                        format!(
                            "Merge has {count} conflicted file(s). \
                             `opencode` CLI is not on PATH — install it from \
                             https://opencode.ai then retry. \
                             Pull Request #{number} was NOT updated."
                        ),
                    );
                }
                UpdatePullRequestOutcome::FetchFailed(detail) => {
                    self.show_toast(
                        ToastVariant::Error,
                        format!(
                            "Failed to fetch remotes while updating PR #{number}: {}",
                            truncate_error(&detail)
                        ),
                    );
                }
                UpdatePullRequestOutcome::MergeFailed(detail) => {
                    self.show_toast(
                        ToastVariant::Error,
                        format!(
                            "Failed to merge `{base_ref}` into PR #{number}: {}",
                            truncate_error(&detail)
                        ),
                    );
                }
                UpdatePullRequestOutcome::PushFailed(detail) => {
                    self.show_toast(
                        ToastVariant::Warning,
                        format!(
                            "Merge of `{base_ref}` into PR #{number} succeeded locally, \
                             but push failed — retry the push: {}",
                            truncate_error(&detail)
                        ),
                    );
                }
                UpdatePullRequestOutcome::AbortFailed(detail) => {
                    let message = if local_only {
                        format!("Failed to abort the merge: {}", truncate_error(&detail))
                    } else {
                        format!(
                            "Failed to abort AI merge for PR #{number}: {}",
                            truncate_error(&detail)
                        )
                    };
                    self.show_toast(ToastVariant::Error, message);
                }
            },
            Err(failure) => {
                self.show_toast(
                    ToastVariant::Error,
                    format!(
                        "Failed to update Pull Request #{}: {}",
                        failure.number,
                        truncate_error(&failure.message)
                    ),
                );
            }
        }
        self.update_pr = None;
        self.enter_screen(Screen::Dashboard, tx);
    }

    fn apply_explain_pr_base_ref(&mut self, base_ref: Option<String>) {
        let Some(screen) = self.explain_pr.as_mut() else {
            return;
        };
        match base_ref {
            Some(base_ref) => screen.set_base_ref(base_ref),
            None => screen.set_error(
                "No base ref reachable (looked for upstream/main, upstream/master, \
                 origin/main, origin/master)."
                    .to_string(),
            ),
        }
    }

    /// Handle the read-only preparation result. `HandedOffToUi` spawns
    /// opencode inside the screen's PTY; every other variant is terminal and
    /// toasts back to the dashboard.
    fn apply_explain_pr_prepared(
        &mut self,
        result: Result<Box<ExplainPreparation>, String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        if self.explain_pr.is_none() {
            return;
        }
        match result {
            Ok(prep) => match *prep {
                ExplainPreparation::HandedOffToUi {
                    opencode_binary,
                    opencode_args,
                    cwd,
                    ..
                } => {
                    if let Some(screen) = self.explain_pr.as_mut() {
                        // Watcher must exist before the spawn so its start
                        // timestamp precedes the session row opencode creates.
                        self.explain_draft = Some(OpencodeTurnWatcher::new(&cwd));
                        screen.spawn_opencode_pty(opencode_binary, opencode_args, cwd, Vec::new());
                    }
                }
                ExplainPreparation::NothingToDescribe => {
                    self.show_toast(
                        ToastVariant::Info,
                        "No commits ahead of the base ref — nothing to describe yet.".to_string(),
                    );
                    self.explain_pr = None;
                    self.enter_screen(Screen::Dashboard, tx);
                }
                ExplainPreparation::AiNotConfigured => {
                    self.show_toast(
                        ToastVariant::Warning,
                        "Set the `ai.explain` model (Settings → Dashboard → ai) so we can draft the PR description with AI."
                            .to_string(),
                    );
                    self.explain_pr = None;
                    self.enter_screen(Screen::Dashboard, tx);
                }
                ExplainPreparation::AiUnavailable => {
                    self.show_toast(
                        ToastVariant::Error,
                        "`opencode` CLI is not on PATH — install it from \
                         https://opencode.ai then retry."
                            .to_string(),
                    );
                    self.explain_pr = None;
                    self.enter_screen(Screen::Dashboard, tx);
                }
            },
            Err(message) => {
                self.show_toast(
                    ToastVariant::Error,
                    format!("Failed to prepare PR draft: {}", truncate_error(&message)),
                );
                self.explain_pr = None;
                self.enter_screen(Screen::Dashboard, tx);
            }
        }
    }

    fn apply_explain_pr_submitted(
        &mut self,
        result: Result<ExplainSubmitOutcome, String>,
        _tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(message) => ExplainSubmitOutcome::SubmitFailed(message),
        };
        if let Some(screen) = self.explain_pr.as_mut() {
            screen.enter_done(outcome);
        }
    }

    fn apply_merge_pr_finished(
        &mut self,
        result: Result<u64, MergePrFailure>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        match result {
            Ok(number) => {
                self.show_toast(
                    ToastVariant::Success,
                    format!("Pull Request #{number} squash-merged."),
                );
            }
            Err(failure) => {
                let trimmed = failure.message.trim();
                let snippet: String = trimmed.chars().take(160).collect();
                let suffix = if trimmed.chars().count() > 160 {
                    "…"
                } else {
                    ""
                };
                self.show_toast(
                    ToastVariant::Error,
                    format!(
                        "Failed to merge Pull Request #{}: {}{}",
                        failure.number, snippet, suffix
                    ),
                );
            }
        }
        self.merge_pr = None;
        // Routing through `enter_screen` rebuilds the Dashboard so the
        // freshly merged row re-fetches and the Merge action disappears.
        self.enter_screen(Screen::Dashboard, tx);
    }

    fn apply_close_pr_finished(&mut self, result: Result<u64, String>) {
        match result {
            Ok(number) => {
                self.show_toast(
                    ToastVariant::Success,
                    format!("Pull Request #{number} closed."),
                );
            }
            Err(message) => {
                let trimmed = message.trim();
                let snippet: String = trimmed.chars().take(160).collect();
                let suffix = if trimmed.chars().count() > 160 {
                    "…"
                } else {
                    ""
                };
                self.show_toast(
                    ToastVariant::Error,
                    format!("Failed to close Pull Request: {snippet}{suffix}"),
                );
            }
        }
        if let Some(watch) = self.dashboard_watch.as_ref() {
            watch.refresh();
        }
    }

    fn apply_init_outcome(&mut self, outcome: InitOutcome, tx: &mpsc::UnboundedSender<AppEvent>) {
        self.git_root = outcome.git_root;
        match outcome.result {
            Ok(service) => {
                self.worktree_service = Some(service);
                let tx2 = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let status = detect_shell_integration();
                    let _ = tx2.send(AppEvent::ShellIntegrationDetected(status));
                });
                self.error = None;
                self.phase = InitPhase::Ready;
                self.enter_screen(self.screen, tx);
            }
            Err(message) => {
                self.error = Some(message);
                self.phase = InitPhase::Errored;
            }
        }
    }

    fn enter_screen(&mut self, screen: Screen, tx: &mpsc::UnboundedSender<AppEvent>) {
        // Delete renders the confirmation modal as an overlay on top of
        // the dashboard. Preserve the dashboard instance across the
        // transition so the row being deleted stays visible behind the
        // modal instead of blanking out.
        let preserved_dashboard = if matches!(screen, Screen::Delete) {
            self.dashboard.take()
        } else {
            None
        };
        self.clear_screen_state();
        self.dashboard = preserved_dashboard;
        self.screen = screen;

        match screen {
            Screen::Menu => {
                self.menu = Some(self.build_menu_screen());
            }
            Screen::Dashboard => {
                let Some(git_root) = self.git_root.as_ref().map(PathBuf::from) else {
                    return;
                };
                let config = self
                    .current_config()
                    .map(|cfg| cfg.dashboard.clone())
                    .unwrap_or_default();
                let mut warnings = self.current_config_warnings();
                let has_terminal_command = self
                    .current_config()
                    .map(|cfg| !cfg.terminal_command.trim().is_empty())
                    .unwrap_or(false);
                let service = DashboardService::new(git_root, config.clone());
                let gh_warning = default_dashboard_warning(&config, service.gh_available());
                let (columns, runtime_warnings) =
                    resolve_dashboard_columns(&config.columns, service.pr_enrichment_enabled());
                warnings.extend(runtime_warnings);
                if let Some(warning) = gh_warning {
                    warnings.push(warning);
                }
                self.dashboard = Some(DashboardScreen::new(
                    self.is_from_wrapper,
                    has_terminal_command,
                    clipboard_available(),
                    columns,
                    warnings,
                    service.pr_enrichment_enabled(),
                ));
                self.dashboard_watch = Some(service.watch());
                self.dashboard_notification_snapshot = None;
            }
            Screen::Cache => {
                self.cache = Some(CacheScreen::new());
                kick_off_cache_load(self.git_root.clone(), tx.clone());
            }
            Screen::Create => {
                self.create = Some(CreateScreen::new());
                kick_off_create_branch_load(self.git_root.clone(), tx.clone());
            }
            Screen::Delete => {
                let delete_branch_with_worktree = self
                    .current_config()
                    .map(|cfg| cfg.delete_branch_with_worktree)
                    .unwrap_or(false);
                self.delete = Some(DeleteScreen::new(delete_branch_with_worktree));
                kick_off_delete_load(self.git_root.clone(), tx.clone());
            }
            Screen::Settings => {
                let local_path = self.local_config_path_str();
                let global_path = global_config_file().display().to_string();
                let has_setup_project = self.git_root.is_some() && !self.has_local_config();
                let settings = match self.settings_snapshot() {
                    Ok((config, config_path)) => SettingsScreen::new(config, config_path)
                        .with_global_config_path(global_path)
                        .with_local_config_path(local_path)
                        .with_has_setup_project(has_setup_project),
                    Err(err) => {
                        let mut settings = SettingsScreen::new(
                            WorktreeConfig::default(),
                            global_config_file().display().to_string(),
                        )
                        .with_global_config_path(global_config_file().display().to_string())
                        .with_local_config_path(local_path)
                        .with_has_setup_project(has_setup_project);
                        settings.set_error(err);
                        settings
                    }
                };
                self.settings = Some(settings);
            }
            Screen::Setup => {
                self.setup = Some(SetupScreen::new(self.shell_integration_status.as_ref()));
            }
            Screen::SetupProject => {
                let root = self.git_root.as_ref().map(PathBuf::from);
                self.setup_project = Some(SetupProjectScreen::new(root.as_deref()));
            }
            Screen::MergePullRequest => {
                // Entered explicitly from `DashboardAction::MergePullRequest`,
                // which seeds `merge_pr` before flipping the screen. If we
                // got here some other way (e.g. user navigated manually),
                // bail back to the menu rather than render an empty shell.
                if self.merge_pr.is_none() {
                    self.back_to_menu();
                }
            }
            Screen::UpdatePullRequest => {
                // Same guard as MergePullRequest: only reachable through
                // `start_update_pr_flow`, which seeds `update_pr` before
                // flipping the screen.
                if self.update_pr.is_none() {
                    self.back_to_menu();
                }
            }
            Screen::ExplainPullRequest => {
                // Only reachable through `start_explain_pr_flow`, which seeds
                // `explain_pr` before flipping the screen.
                if self.explain_pr.is_none() {
                    self.back_to_menu();
                }
            }
            Screen::FixPullRequest => {
                // Only reachable through `start_fix_pr_flow`, which seeds
                // `fix_pr` before flipping the screen.
                if self.fix_pr.is_none() {
                    self.back_to_menu();
                }
            }
            Screen::ReviewPullRequest => {
                // Only reachable through `start_review_pr_flow`, which seeds
                // `review_pr` before flipping the screen.
                if self.review_pr.is_none() {
                    self.back_to_menu();
                }
            }
            Screen::BugkillPullRequest => {
                // Only reachable through `start_bugkill_flow`, which seeds
                // `bugkill_pr` before flipping the screen.
                if self.bugkill_pr.is_none() {
                    self.back_to_menu();
                }
            }
            Screen::UpdateBranch => {
                // Only reachable through `start_update_branch_flow`,
                // which seeds `update_branch` before flipping the
                // screen. Any other path means we lost the splash and
                // would render an empty panel — bail back to the menu.
                if self.update_branch.is_none() {
                    self.back_to_menu();
                }
            }
            Screen::AiModelPicker => {
                // The picker is opened as a modal overlay via
                // `open_ai_model_picker`, not through `enter_screen`. Hitting
                // this arm means we lost the underlying Settings state — bail
                // back to the menu rather than render an empty panel.
                if self.ai_model_picker.is_none() {
                    self.back_to_menu();
                }
            }
        }

        if !matches!(screen, Screen::Delete) {
            self.pending_delete_path = None;
            self.pending_bulk_delete_paths.clear();
            self.bulk_delete_queue.clear();
        }
    }

    fn clear_screen_state(&mut self) {
        self.menu = None;
        self.cache = None;
        self.dashboard = None;
        self.dashboard_watch = None;
        self.dashboard_notification_snapshot = None;
        self.cache = None;
        self.create = None;
        self.delete = None;
        self.settings = None;
        self.setup = None;
        self.setup_project = None;
        self.merge_pr = None;
        self.update_pr = None;
        self.explain_pr = None;
        self.fix_pr = None;
        self.bugkill_pr = None;
        self.update_branch = None;
        self.ai_model_picker = None;
        self.mouse_selection = None;
    }

    fn back_to_menu(&mut self) {
        self.clear_screen_state();
        self.screen = Screen::Menu;
        self.pending_delete_path = None;
        self.pending_bulk_delete_paths.clear();
        self.bulk_delete_queue.clear();
        self.menu = Some(self.build_menu_screen());
    }

    /// Return from a cancelled PR-command screen (Merge/Update/Explain/Fix/
    /// Bugkill) to the dashboard's action menu for the worktree the command
    /// was launched from, instead of the bare table. Cancel only ever fires
    /// before any git state changes, so the dashboard's already-loaded rows
    /// are still accurate — no rebuild/refetch needed. Falls back to a fresh
    /// dashboard if the underlying instance was somehow lost.
    fn back_to_dashboard_action_menu(
        &mut self,
        worktree_path: Option<String>,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        match (self.dashboard.as_mut(), worktree_path) {
            (Some(dashboard), Some(path)) => {
                self.screen = Screen::Dashboard;
                dashboard.reopen_action_menu_for_worktree(&path);
            }
            _ => self.enter_screen(Screen::Dashboard, tx),
        }
    }

    fn finish_create_success(&mut self) {
        let navigate = self
            .create
            .as_ref()
            .map(|c| c.navigate_after_create)
            .unwrap_or(false);
        let path = self
            .create
            .as_ref()
            .and_then(|c| c.created_worktree_path().map(str::to_string));

        self.show_toast(ToastVariant::Success, CREATE_SUCCESS);

        if navigate {
            if let Some(path) = path {
                if self.is_from_wrapper {
                    self.selected_path = Some(path);
                    self.quit_requested = true;
                    return;
                }
                let (new_branch, source_branch) = self
                    .create
                    .as_ref()
                    .map(|c| (c.new_branch.clone(), c.source_branch.clone()))
                    .unwrap_or_default();
                if let Some(config) = self.current_config() {
                    if !config.terminal_command.trim().is_empty() {
                        let mut variables = self.terminal_template_variables(&path, &new_branch);
                        variables.source_branch = source_branch;
                        let _ = open_terminal(&config.terminal_command, &variables);
                    }
                }
            }
        }

        self.back_to_menu();
    }

    fn poll_dashboard_updates(&mut self) {
        let (updates_batch, notices) = {
            let Some(watch) = self.dashboard_watch.as_mut() else {
                return;
            };
            let mut updates_batch = Vec::new();
            let mut notices = Vec::new();
            while let Ok(update) = watch.rx.try_recv() {
                updates_batch.push(update);
            }
            while let Ok(notice) = watch.notice_rx.try_recv() {
                notices.push(notice);
            }
            (updates_batch, notices)
        };

        let notifications = self.current_notifications_config();
        let mut should_ring_bell = false;
        for update in updates_batch {
            if dashboard_update_requests_bell(
                &mut self.dashboard_notification_snapshot,
                &update,
                &notifications,
            ) {
                should_ring_bell = true;
            }

            if let Some(screen) = self.dashboard.as_mut() {
                if let DashboardUpdate::WithPRs {
                    next_pr_fetch_at, ..
                } = &update
                {
                    screen.set_next_pr_fetch_at(*next_pr_fetch_at);
                }
                screen.set_rows(update.into_rows());
            }
        }
        if should_ring_bell {
            terminal::ring_bell();
        }
        let has_rows = self
            .dashboard
            .as_ref()
            .is_some_and(DashboardScreen::has_rows);
        let mut refresh_dashboard = false;
        for notice in notices {
            if notice.level == DashboardNoticeLevel::Success {
                refresh_dashboard = true;
                self.show_toast(ToastVariant::Success, notice.message);
                continue;
            }
            if let Some(screen) = self.dashboard.as_mut() {
                if has_rows {
                    screen.set_notice(notice);
                } else {
                    screen.set_error(notice.message);
                }
            }
        }
        if refresh_dashboard {
            if let Some(watch) = self.dashboard_watch.as_ref() {
                watch.refresh();
            }
        }
    }

    fn show_toast(&mut self, variant: ToastVariant, message: impl Into<String>) {
        self.toast.show(message, variant);
    }

    fn build_menu_screen(&self) -> MenuScreen {
        MenuScreen::new(
            self.last_menu_index,
            self.git_root.clone(),
            self.shell_integration_status
                .as_ref()
                .map(|status| status.is_installed),
            self.current_config()
                .map(|config| !config.worktree_link_patterns.is_empty())
                .unwrap_or(false),
        )
    }

    fn has_local_config(&self) -> bool {
        self.local_config_path()
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    fn current_config(&self) -> Option<&WorktreeConfig> {
        self.worktree_service
            .as_ref()
            .map(|service| service.config_service().config())
    }

    /// Build the `TemplateVariables` for spawning the user's `terminalCommand`
    /// outside of the create flow. `branch` may be empty when the caller
    /// doesn't have it (e.g. a detached worktree).
    fn terminal_template_variables(&self, worktree_path: &str, branch: &str) -> TemplateVariables {
        let base_path = self
            .git_root
            .as_deref()
            .map(std::path::Path::new)
            .map(repository_base_name)
            .unwrap_or_default();
        TemplateVariables {
            base_path,
            worktree_path: worktree_path.to_string(),
            branch_name: branch.to_string(),
            source_branch: String::new(),
        }
    }

    fn current_config_warnings(&self) -> Vec<String> {
        self.worktree_service
            .as_ref()
            .map(|service| service.config_service().warnings().to_vec())
            .unwrap_or_default()
    }

    fn settings_snapshot(&self) -> Result<(WorktreeConfig, String), String> {
        if let Some(service) = self.worktree_service.as_ref() {
            let config_service = service.config_service();
            if let Some(path) = config_service.config_path() {
                return Ok((config_service.config().clone(), path.display().to_string()));
            }
        }

        let mut config_service = ConfigService::new();
        let config = config_service.load_global().map_err(|e| e.to_string())?;
        let path = config_service
            .config_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| global_config_file().display().to_string());
        Ok((config, path))
    }

    fn active_config_uses_global(&self) -> bool {
        let global_path = global_config_file();
        self.worktree_service
            .as_ref()
            .and_then(|service| service.config_service().config_path())
            .map(|path| path == global_path.as_path())
            .unwrap_or(false)
    }

    fn save_delete_branch_with_worktree(&mut self, enabled: bool) -> Result<(), String> {
        let local_path = self.local_config_path();
        let target_path = match local_path.as_ref().filter(|p| p.exists()) {
            Some(path) => path.clone(),
            None => global_config_file(),
        };

        let mut reader = ConfigService::new();
        let mut config = if target_path.exists() {
            if target_path == global_config_file() {
                reader.load_global().map_err(|e| e.to_string())?
            } else {
                reader
                    .load(target_path.parent())
                    .map_err(|e| e.to_string())?
            }
        } else {
            WorktreeConfig::default()
        };
        config.delete_branch_with_worktree = enabled;

        let mut writer = ConfigService::new();
        writer
            .save(&config, Some(&target_path))
            .map_err(|e| e.to_string())?;

        if let Some(service) = self.worktree_service.as_mut() {
            let project_path = local_path.as_ref().and_then(|p| p.parent());
            service
                .config_service_mut()
                .load(project_path)
                .map_err(|e| e.to_string())?;
        }

        let path = target_path.display().to_string();

        if let Some(settings) = self.settings.as_mut() {
            settings.set_config(config, path);
        }
        Ok(())
    }

    fn local_config_path(&self) -> Option<PathBuf> {
        self.git_root
            .as_ref()
            .map(|root| PathBuf::from(root).join(LOCAL_CONFIG_FILE_NAME))
    }

    fn local_config_path_str(&self) -> Option<String> {
        self.local_config_path().map(|p| p.display().to_string())
    }

    fn settings_edit_file_path(&self) -> PathBuf {
        self.local_config_path()
            .filter(|path| path.exists())
            .unwrap_or_else(global_config_file)
    }

    fn save_post_create_commands(&mut self, commands: Vec<String>) -> Result<(), String> {
        let local_path = self
            .local_config_path()
            .ok_or_else(|| "No git repository in scope".to_string())?;

        let mut config = if local_path.exists() {
            let mut svc = ConfigService::new();
            svc.load(local_path.parent()).map_err(|e| e.to_string())?
        } else {
            self.current_config().cloned().unwrap_or_default()
        };
        config.post_create_cmd = commands.clone();

        let mut writer = ConfigService::new();
        writer
            .save(&config, Some(&local_path))
            .map_err(|e| e.to_string())?;

        if let Some(service) = self.worktree_service.as_mut() {
            service
                .config_service_mut()
                .load(local_path.parent())
                .map_err(|e| e.to_string())?;
        }

        if let Some(settings) = self.settings.as_mut() {
            settings.mark_post_create_commands_saved(commands);
        }
        Ok(())
    }

    fn save_copy_patterns(&mut self, patterns: Vec<String>) -> Result<(), String> {
        self.save_pattern_list_setting(
            |config| config.worktree_copy_patterns = patterns.clone(),
            |settings| settings.mark_copy_patterns_saved(patterns.clone()),
        )
    }

    fn save_ignore_patterns(&mut self, patterns: Vec<String>) -> Result<(), String> {
        self.save_pattern_list_setting(
            |config| config.worktree_copy_ignores = patterns.clone(),
            |settings| settings.mark_ignore_patterns_saved(patterns.clone()),
        )
    }

    fn save_link_patterns(&mut self, patterns: Vec<String>) -> Result<(), String> {
        self.save_pattern_list_setting(
            |config| config.worktree_link_patterns = patterns.clone(),
            |settings| settings.mark_link_patterns_saved(patterns.clone()),
        )
    }

    fn save_pattern_list_setting<F, G>(
        &mut self,
        mut apply: F,
        mut mark_saved: G,
    ) -> Result<(), String>
    where
        F: FnMut(&mut WorktreeConfig),
        G: FnMut(&mut SettingsScreen),
    {
        let local_path = self.local_config_path();
        let target_path = match local_path.as_ref().filter(|p| p.exists()) {
            Some(path) => path.clone(),
            None => global_config_file(),
        };

        let mut reader = ConfigService::new();
        let mut config = if target_path.exists() {
            reader
                .load(target_path.parent())
                .map_err(|e| e.to_string())?
        } else {
            WorktreeConfig::default()
        };
        apply(&mut config);

        let mut writer = ConfigService::new();
        writer
            .save(&config, Some(&target_path))
            .map_err(|e| e.to_string())?;

        if let Some(service) = self.worktree_service.as_mut() {
            let project_path = local_path.as_ref().and_then(|p| p.parent());
            service
                .config_service_mut()
                .load(project_path)
                .map_err(|e| e.to_string())?;
        }

        if let Some(settings) = self.settings.as_mut() {
            mark_saved(settings);
        }
        Ok(())
    }

    fn save_terminal_command(&mut self, command: String) -> Result<(), String> {
        let local_path = self.local_config_path();
        let target_path = match local_path.as_ref().filter(|p| p.exists()) {
            Some(path) => path.clone(),
            None => global_config_file(),
        };

        let mut reader = ConfigService::new();
        let mut config = if target_path.exists() {
            reader
                .load(target_path.parent())
                .map_err(|e| e.to_string())?
        } else {
            WorktreeConfig::default()
        };
        config.terminal_command = command.clone();

        let mut writer = ConfigService::new();
        writer
            .save(&config, Some(&target_path))
            .map_err(|e| e.to_string())?;

        if let Some(service) = self.worktree_service.as_mut() {
            let project_path = local_path.as_ref().and_then(|p| p.parent());
            service
                .config_service_mut()
                .load(project_path)
                .map_err(|e| e.to_string())?;
        }

        if let Some(settings) = self.settings.as_mut() {
            settings.mark_terminal_command_saved(command);
        }
        Ok(())
    }

    fn save_path_template(&mut self, template: String) -> Result<(), String> {
        let local_path = self.local_config_path();
        let target_path = match local_path.as_ref().filter(|p| p.exists()) {
            Some(path) => path.clone(),
            None => global_config_file(),
        };

        let mut reader = ConfigService::new();
        let mut config = if target_path.exists() {
            reader
                .load(target_path.parent())
                .map_err(|e| e.to_string())?
        } else {
            WorktreeConfig::default()
        };
        config.worktree_path_template = template.clone();

        let mut writer = ConfigService::new();
        writer
            .save(&config, Some(&target_path))
            .map_err(|e| e.to_string())?;

        if let Some(service) = self.worktree_service.as_mut() {
            let project_path = local_path.as_ref().and_then(|p| p.parent());
            service
                .config_service_mut()
                .load(project_path)
                .map_err(|e| e.to_string())?;
        }

        if let Some(settings) = self.settings.as_mut() {
            settings.mark_path_template_saved(template);
        }
        Ok(())
    }

    fn save_link_strategy(&mut self, strategy: LinkStrategy) -> Result<(), String> {
        let local_path = self.local_config_path();
        let target_path = match local_path.as_ref().filter(|p| p.exists()) {
            Some(path) => path.clone(),
            None => global_config_file(),
        };

        let mut reader = ConfigService::new();
        let mut config = if target_path.exists() {
            reader
                .load(target_path.parent())
                .map_err(|e| e.to_string())?
        } else {
            WorktreeConfig::default()
        };
        config.worktree_link_strategy = strategy;

        let mut writer = ConfigService::new();
        writer
            .save(&config, Some(&target_path))
            .map_err(|e| e.to_string())?;

        if let Some(service) = self.worktree_service.as_mut() {
            let project_path = local_path.as_ref().and_then(|p| p.parent());
            service
                .config_service_mut()
                .load(project_path)
                .map_err(|e| e.to_string())?;
        }

        if let Some(settings) = self.settings.as_mut() {
            settings.mark_link_strategy_saved(strategy);
        }
        Ok(())
    }

    fn save_link_cache_dir(&mut self, cache_dir: String) -> Result<(), String> {
        let local_path = self.local_config_path();
        let target_path = match local_path.as_ref().filter(|p| p.exists()) {
            Some(path) => path.clone(),
            None => global_config_file(),
        };

        let mut reader = ConfigService::new();
        let mut config = if target_path.exists() {
            reader
                .load(target_path.parent())
                .map_err(|e| e.to_string())?
        } else {
            WorktreeConfig::default()
        };
        let trimmed = cache_dir.trim().to_string();
        config.worktree_link_cache_dir = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.clone())
        };

        let mut writer = ConfigService::new();
        writer
            .save(&config, Some(&target_path))
            .map_err(|e| e.to_string())?;

        if let Some(service) = self.worktree_service.as_mut() {
            let project_path = local_path.as_ref().and_then(|p| p.parent());
            service
                .config_service_mut()
                .load(project_path)
                .map_err(|e| e.to_string())?;
        }

        if let Some(settings) = self.settings.as_mut() {
            settings.mark_link_cache_dir_saved(config.worktree_link_cache_dir.clone());
        }
        Ok(())
    }

    fn save_dashboard(&mut self, dashboard: DashboardConfig) -> Result<(), String> {
        let local_path = self.local_config_path();
        let wise_merge_changed = dashboard.wise_merge != self.current_dashboard_config().wise_merge;
        let target_path = if wise_merge_changed {
            local_path
                .clone()
                .ok_or_else(|| "No git repository in scope".to_string())?
        } else {
            match local_path.as_ref().filter(|p| p.exists()) {
                Some(path) => path.clone(),
                None => global_config_file(),
            }
        };

        let mut reader = ConfigService::new();
        let mut config = if target_path.exists() {
            reader
                .load(target_path.parent())
                .map_err(|e| e.to_string())?
        } else if wise_merge_changed {
            self.current_config().cloned().unwrap_or_default()
        } else {
            WorktreeConfig::default()
        };
        config.dashboard = dashboard.clone();

        let mut writer = ConfigService::new();
        writer
            .save(&config, Some(&target_path))
            .map_err(|e| e.to_string())?;

        if let Some(service) = self.worktree_service.as_mut() {
            let project_path = local_path.as_ref().and_then(|p| p.parent());
            service
                .config_service_mut()
                .load(project_path)
                .map_err(|e| e.to_string())?;
        }

        if let Some(settings) = self.settings.as_mut() {
            settings.mark_dashboard_saved(dashboard);
        }
        Ok(())
    }

    fn save_notifications(&mut self, notifications: NotificationsConfig) -> Result<(), String> {
        let local_path = self.local_config_path();
        let target_path = match local_path.as_ref().filter(|p| p.exists()) {
            Some(path) => path.clone(),
            None => global_config_file(),
        };

        let mut reader = ConfigService::new();
        let mut config = if target_path.exists() {
            reader
                .load(target_path.parent())
                .map_err(|e| e.to_string())?
        } else {
            WorktreeConfig::default()
        };
        config.notifications = notifications.clone();

        let mut writer = ConfigService::new();
        writer
            .save(&config, Some(&target_path))
            .map_err(|e| e.to_string())?;

        if let Some(service) = self.worktree_service.as_mut() {
            let project_path = local_path.as_ref().and_then(|p| p.parent());
            service
                .config_service_mut()
                .load(project_path)
                .map_err(|e| e.to_string())?;
        }

        if let Some(settings) = self.settings.as_mut() {
            settings.mark_notifications_saved(notifications);
        }
        Ok(())
    }

    fn copy_settings(&mut self, direction: CopyDirection) -> Result<(), String> {
        let local_path = self
            .local_config_path()
            .ok_or_else(|| "No git repository in scope".to_string())?;
        let global_path = global_config_file();

        let config = match direction {
            CopyDirection::GlobalToLocal => {
                let mut reader = ConfigService::new();
                reader.load_global().map_err(|e| e.to_string())?
            }
            CopyDirection::LocalToGlobal => {
                if !local_path.exists() {
                    return Err(format!(
                        "No project-local config found at {}",
                        local_path.display()
                    ));
                }
                let mut reader = ConfigService::new();
                reader
                    .load(local_path.parent())
                    .map_err(|e| e.to_string())?
            }
        };

        let target_path = match direction {
            CopyDirection::GlobalToLocal => local_path.clone(),
            CopyDirection::LocalToGlobal => global_path.clone(),
        };

        let mut writer = ConfigService::new();
        writer
            .save(&config, Some(target_path.as_path()))
            .map_err(|e| e.to_string())?;

        if let Some(service) = self.worktree_service.as_mut() {
            service
                .config_service_mut()
                .load(local_path.parent())
                .map_err(|e| e.to_string())?;
        }

        self.refresh_settings_screen()
    }

    fn refresh_settings_screen(&mut self) -> Result<(), String> {
        let (config, path) = self.settings_snapshot()?;

        if let Some(settings) = self.settings.as_mut() {
            settings.set_config(config, path);
        }
        Ok(())
    }

    fn reset_settings_config(&mut self) -> Result<(), String> {
        let mut config_service = ConfigService::new();
        config_service
            .create_global_config()
            .map_err(|e| e.to_string())?;

        if self.active_config_uses_global() {
            let service = self
                .worktree_service
                .as_mut()
                .ok_or_else(|| "Worktree service not initialized".to_string())?;
            service
                .config_service_mut()
                .load_global()
                .map_err(|e| e.to_string())?;
        }

        let config = config_service.config().clone();
        let path = config_service
            .config_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| global_config_file().display().to_string());

        if let Some(settings) = self.settings.as_mut() {
            settings.set_config(config, path);
        }
        Ok(())
    }
}

struct InitOutcome {
    git_root: Option<String>,
    result: Result<WorktreeService, String>,
}

/// Listen for terminal-related signals (SIGTERM/SIGINT/SIGQUIT/SIGHUP) and
/// flip a shared flag when any of them arrives. The main event loop checks
/// the flag every tick and breaks out cleanly, which routes the shutdown
/// through the normal Drop chain — including crossterm's
/// `DisableMouseCapture` and `disable_raw_mode`, so the user's terminal is
/// returned to a sane state.
///
/// On Linux there is a secondary fallback: crossterm's mio backend can
/// enter an infinite inner read-loop when the PTY master closes (EIO is
/// silently dropped without `break`), so the cooperative tokio-signal path
/// never gets a chance to run. A dedicated OS thread polls `STDIN_FILENO`
/// for `POLLHUP` with a raw `libc::poll()` call. When triggered it runs
/// terminal cleanup and calls `process::exit` directly, bypassing the stuck
/// crossterm loop.
fn install_termination_listener() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    {
        let flag_for_signal = flag.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let Ok(mut term) = signal(SignalKind::terminate()) else {
                return;
            };
            let Ok(mut int) = signal(SignalKind::interrupt()) else {
                return;
            };
            let Ok(mut quit) = signal(SignalKind::quit()) else {
                return;
            };
            let Ok(mut hup) = signal(SignalKind::hangup()) else {
                return;
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
                _ = quit.recv() => {}
                _ = hup.recv() => {}
            }
            flag_for_signal.store(true, Ordering::Relaxed);
        });

        // Only install the watchdog when stdin is a real TTY. Piped or
        // redirected stdin would trigger POLLHUP immediately and cause a
        // spurious exit before any user interaction.
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            let flag_for_watchdog = flag.clone();
            std::thread::spawn(move || {
                loop {
                    let mut pfd = libc::pollfd {
                        fd: libc::STDIN_FILENO,
                        // events = POLLIN: macOS's poll only reports POLLHUP
                        // when at least one event flag is requested. With
                        // events = 0 the slave-end of a closed-master PTY
                        // never surfaces POLLHUP, so the watchdog can't see
                        // the hangup. POLLIN is harmless — the main
                        // crossterm loop has its own read on STDIN_FILENO
                        // and they coexist (multiple pollers on the same fd
                        // is supported by every Unix).
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    unsafe { libc::poll(&mut pfd, 1, 250) };

                    // Order matters: check POLLHUP *before* the quit flag.
                    // The cooperative SIGHUP handler also sets the flag, and
                    // when both the signal and the hangup fire together
                    // (terminal closes → both POLLHUP on stdin AND SIGHUP
                    // on the controlling tty), an "if flag, return" check
                    // first would defer to the cooperative path, which is
                    // gated behind the sync `event::poll` inside the event
                    // loop and can take seconds to wake. We must force-exit
                    // on POLLHUP unconditionally so dashboard renders never
                    // outlive their terminal.
                    if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                        let _ = crossterm::terminal::disable_raw_mode();
                        std::process::exit(0);
                    }

                    // Cooperative quit (user pressed q, etc.) already drove
                    // a clean shutdown — stop polling so we don't burn CPU.
                    if flag_for_watchdog.load(Ordering::Relaxed) {
                        return;
                    }
                }
            });
        }
    }
    flag
}

fn kick_off_initialize(tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let git_root = get_git_root(None).await;
        let working_dir = git_root.clone().map(PathBuf::from);
        let mut service = WorktreeService::new(working_dir);
        let result = match service.initialize().await {
            Ok(()) => Ok(service),
            Err(e) => Err(user_friendly_message(&e)),
        };
        let _ = tx.send(AppEvent::Initialized(Box::new(InitOutcome {
            git_root,
            result,
        })));
    });
}

fn kick_off_cache_load(git_root: Option<String>, tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let mut service = WorktreeService::new(git_root.map(PathBuf::from));
        if let Err(err) = service.initialize().await {
            let _ = tx.send(AppEvent::CacheLoaded(Err(user_friendly_message(&err))));
            return;
        }

        let result = service
            .cache_overview()
            .await
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::CacheLoaded(result));
    });
}

fn kick_off_cache_entry_delete(
    git_root: Option<String>,
    relative_path: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let mut service = WorktreeService::new(git_root.map(PathBuf::from));
        if let Err(err) = service.initialize().await {
            let _ = tx.send(AppEvent::CacheEntryDeleted(Err(user_friendly_message(
                &err,
            ))));
            return;
        }

        let result = match service.remove_repo_cache_entry(&relative_path).await {
            Ok(()) => service
                .cache_overview()
                .await
                .map_err(|err| user_friendly_message(&err)),
            Err(err) => Err(user_friendly_message(&err)),
        };
        let _ = tx.send(AppEvent::CacheEntryDeleted(result));
    });
}

fn kick_off_create_branch_load(git_root: Option<String>, tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let service = GitService::new(git_root.map(PathBuf::from));
        let result = service
            .list_branches()
            .await
            .map_err(|e| user_friendly_message(&e));
        let _ = tx.send(AppEvent::CreateBranchesLoaded(result));
    });
}

fn kick_off_delete_load(git_root: Option<String>, tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let service = GitService::new(git_root.map(PathBuf::from));
        let result = service
            .list_worktrees()
            .await
            .map_err(|e| user_friendly_message(&e));
        let _ = tx.send(AppEvent::DeleteLoaded(result));
    });
}

fn kick_off_wise_preset_discovery(git_root: Option<String>, tx: mpsc::UnboundedSender<AppEvent>) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::WisePresetDiscovered(Err(
            "Could not resolve the current repository root.".to_string(),
        )));
        return;
    };

    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            crate::services::presets::discover_wise(&root)
                .ok_or_else(|| "Could not scan the current repository.".to_string())
        })
        .await
        .map_err(|err| err.to_string())
        .and_then(|inner| inner);
        let _ = tx.send(AppEvent::WisePresetDiscovered(result));
    });
}

fn kick_off_create_worktree(
    git_root: Option<String>,
    options: WorktreeCreateOptions,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let mut service = WorktreeService::new(git_root.map(PathBuf::from));
        if let Err(err) = service.initialize().await {
            let _ = tx.send(AppEvent::CreateFinished(Err(user_friendly_message(&err))));
            return;
        }

        let activity_tx = tx.clone();
        let mut on_activity = move |text: &str, kind: crate::files::ActivityKind| {
            let _ = activity_tx.send(AppEvent::CreateActivity {
                text: text.to_string(),
                kind,
            });
        };
        let activity_cb: &mut (dyn FnMut(&str, crate::files::ActivityKind) + Send) =
            &mut on_activity;

        let result = service
            .create_worktree(&options, None, Some(activity_cb))
            .await
            .map_err(|e| user_friendly_message(&e));
        let _ = tx.send(AppEvent::CreateFinished(result));
    });
}

fn kick_off_delete_worktree(
    git_root: Option<String>,
    path: String,
    force: bool,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let mut service = WorktreeService::new(git_root.map(PathBuf::from));
        if let Err(err) = service.initialize().await {
            let _ = tx.send(AppEvent::DeleteFinished(Err(user_friendly_message(&err))));
            return;
        }

        let result = service
            .delete_worktree(&path, force)
            .await
            .map_err(|e| user_friendly_message(&e));
        let _ = tx.send(AppEvent::DeleteFinished(result));
    });
}

fn kick_off_update_check(tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let result = check_for_updates_all_sources(VERSION).await;
        let _ = tx.send(AppEvent::SettingsUpdateChecked(result));
    });
}

fn kick_off_upgrade(source: UpdateSource, tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || run_upgrade(source))
            .await
            .map_err(|err| err.to_string())
            .and_then(|inner| inner);
        let _ = tx.send(AppEvent::SettingsUpgradeFinished { source, result });
    });
}

fn run_upgrade(source: UpdateSource) -> Result<String, String> {
    let argv = source.upgrade_argv();
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| "empty upgrade command".to_string())?;
    let output = std::process::Command::new(program)
        .args(rest)
        .output()
        .map_err(|err| format!("failed to spawn `{program}`: {err}"))?;
    if output.status.success() {
        Ok(format!(
            "upgraded via `{}`",
            source.upgrade_command_display()
        ))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exited with status {}", output.status)
        };
        Err(detail)
    }
}

fn kick_off_fetch_opencode_models(tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let result = match fetch_opencode_models().await {
            Ok(mut models) => {
                // Explain the models.dev catalogue with the authoritative
                // per-model variant sets from the local CLI. Best-effort: if
                // the CLI is missing or errors, every model keeps `variants:
                // None` and the picker falls back to the generic ladder.
                let binary = PathBuf::from(crate::constants::OPENCODE_CLI_BINARY);
                if let Ok(variants) = fetch_opencode_model_variants(&binary).await {
                    for model in &mut models {
                        if let Some(v) = variants.get(&model.pair()) {
                            model.variants = Some(v.clone());
                        }
                    }
                }
                Ok(models)
            }
            Err(message) => Err(message),
        };
        let _ = tx.send(AppEvent::AiModelsFetched(result));
    });
}

/// Shell out to the locally installed `opencode models opencode` to harvest
/// the small subset of "free" provider/model pairs the upstream router is
/// actually willing to serve right now. The Dashboard editor footer renders
/// the result as selectable chips. Uses the default binary name from
/// `crate::constants::OPENCODE_CLI_BINARY` — same lookup the dashboard
/// service uses for the conflict-resolution shell-out.
fn kick_off_fetch_free_opencode_models(tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let binary = PathBuf::from(crate::constants::OPENCODE_CLI_BINARY);
        let result = fetch_free_opencode_models(&binary).await;
        let _ = tx.send(AppEvent::FreeOpencodeModelsFetched(result));
    });
}

/// Shell out to `opencode models --verbose` to learn each model's authoritative
/// reasoning variants, so the AI Settings slots' ←/→ cycle offers only the
/// levels the chosen model actually accepts. Same best-effort posture as the
/// free-model fetch — failures leave the Settings screen on the generic
/// fallback ladder.
fn kick_off_fetch_ai_model_variants(tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let binary = PathBuf::from(crate::constants::OPENCODE_CLI_BINARY);
        let result = fetch_opencode_model_variants(&binary).await;
        let _ = tx.send(AppEvent::AiModelVariantsFetched(result));
    });
}

fn kick_off_clipboard_copy(
    value: String,
    success_message: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || copy_to_clipboard(&value))
            .await
            .map_err(|err| err.to_string())
            .and_then(|inner| inner);
        let _ = tx.send(AppEvent::ClipboardCopyFinished {
            success_message,
            error: result.err(),
        });
    });
}

fn kick_off_fetch_pr_details(
    git_root: Option<String>,
    config: DashboardConfig,
    number: u64,
    worktree_path: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::MergePrDetailsLoaded(Err(
            "Could not resolve git root for PR details fetch.".to_string(),
        )));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        // Detect unpushed local commits alongside the body fetch so the
        // confirm screen can warn before a squash-merge silently drops them.
        let unpushed_commits = service.unpushed_commit_count(&worktree_path).await;
        let result = service
            .fetch_pr_details(number)
            .await
            .map(|details| MergePrDetailsPayload {
                title: details.title,
                body: details.body,
                unpushed_commits,
            })
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::MergePrDetailsLoaded(result));
    });
}

fn kick_off_merge_pull_request(
    git_root: Option<String>,
    config: DashboardConfig,
    exec: MergeExecution,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let number = exec.number;
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::MergePrFinished(Err(MergePrFailure {
            number,
            message: "Could not resolve git root for merge.".to_string(),
        })));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        // Flush local commits into the PR before merging so the squash
        // doesn't drop them. A push failure aborts the merge — better to
        // stop than to merge a stale PR and lose the commits.
        if exec.push_first {
            if let Err(err) = service.push_head_to_origin(&exec.worktree_path).await {
                let _ = tx.send(AppEvent::MergePrFinished(Err(MergePrFailure {
                    number,
                    message: format!(
                        "Failed to push local commits before merge: {}",
                        user_friendly_message(&err)
                    ),
                })));
                return;
            }
        }
        let result = match service
            .merge_pull_request(number, &exec.subject, &exec.body)
            .await
        {
            Ok(()) => Ok(number),
            Err(err) => Err(MergePrFailure {
                number,
                message: user_friendly_message(&err),
            }),
        };
        let _ = tx.send(AppEvent::MergePrFinished(result));
    });
}

fn kick_off_close_pull_request(
    git_root: Option<String>,
    config: DashboardConfig,
    number: u64,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::ClosePrFinished(Err(
            "Could not resolve git root for closing the pull request.".to_string(),
        )));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = match service.close_pull_request(number).await {
            Ok(()) => Ok(number),
            Err(err) => Err(user_friendly_message(&err)),
        };
        let _ = tx.send(AppEvent::ClosePrFinished(result));
    });
}

fn kick_off_resolve_base_ref(
    worktree_path: String,
    number: u64,
    pr_base_ref: Option<String>,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let base_ref = crate::services::dashboard::resolve_base_ref(
            &PathBuf::from(&worktree_path),
            pr_base_ref.as_deref(),
        )
        .await;
        let _ = tx.send(AppEvent::UpdatePrBaseRefResolved { number, base_ref });
    });
}

fn kick_off_resolve_explain_base_ref(
    worktree_path: String,
    pr_base_ref: Option<String>,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let base_ref = crate::services::dashboard::resolve_base_ref(
            &PathBuf::from(&worktree_path),
            pr_base_ref.as_deref(),
        )
        .await;
        let _ = tx.send(AppEvent::ExplainPrBaseRefResolved { base_ref });
    });
}

fn kick_off_prepare_explain(
    git_root: Option<String>,
    config: DashboardConfig,
    request: ExplainPullRequestRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::ExplainPrPrepared(Err(
            "Could not resolve git root for the PR draft.".to_string(),
        )));
        return;
    };
    tokio::spawn(async move {
        // `base_ref` is populated by the resolver before the user can
        // confirm; guard anyway so a race can't blow up the worker.
        let Some(base_ref) = request.base_ref.clone() else {
            let _ = tx.send(AppEvent::ExplainPrPrepared(Err(
                "Base ref was not resolved before confirmation.".to_string(),
            )));
            return;
        };
        let service = DashboardService::new(root, config);
        let event = match service
            .prepare_explain(&request.worktree_path, &request.branch, &base_ref)
            .await
        {
            Ok(prep) => Ok(Box::new(prep)),
            Err(err) => Err(user_friendly_message(&err)),
        };
        let _ = tx.send(AppEvent::ExplainPrPrepared(event));
    });
}

fn kick_off_submit_pull_request(
    git_root: Option<String>,
    config: DashboardConfig,
    params: ExplainSubmitRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::ExplainPrSubmitted(Err(
            "Could not resolve git root for the pull request.".to_string(),
        )));
        return;
    };
    tokio::spawn(async move {
        let (activity_tx, mut activity_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, crate::files::ActivityKind)>();

        // Forward terminal-activity lines into the main event loop.
        let forward_tx = tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some((text, kind)) = activity_rx.recv().await {
                let _ = forward_tx.send(AppEvent::ExplainPrActivity { text, kind });
            }
        });

        let service = DashboardService::new(root, config);
        let event = match service
            .submit_pull_request(&params, Some(&activity_tx))
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(err) => Err(user_friendly_message(&err)),
        };
        drop(activity_tx);
        let _ = forwarder.await;
        let _ = tx.send(AppEvent::ExplainPrSubmitted(event));
    });
}

// ── "Fix Pull Request" async stages ────────────────────────────────────

/// Inputs for one captured planning call. `index` rides along so the result
/// handler can ignore a stale response.
struct FixPlanRequest {
    worktree_path: String,
    group: CommentGroup,
    feedback: Option<String>,
    previous_plan: Option<String>,
    /// Comments + replies + fixes already resolved earlier this run, so the
    /// model can interpret a comment that refers back to them.
    history: Option<String>,
    index: usize,
}

/// Inputs for building the live-apply spawn parameters.
struct FixApplyRequest {
    worktree_path: String,
    group: CommentGroup,
    plan: FixPlan,
    index: usize,
}

/// Inputs for the commit + reply that follow a live apply.
struct FixCommitRequest {
    worktree_path: String,
    owner: String,
    repo: String,
    number: u64,
    pr_url: String,
    comment_index: usize,
    index: usize,
    group: CommentGroup,
    plan: FixPlan,
}

/// Inputs for a non-actionable reply (the `reply` verdict).
struct FixReplyRequest {
    worktree_path: String,
    owner: String,
    repo: String,
    number: u64,
    group: CommentGroup,
    text: String,
    index: usize,
}

fn kick_off_prepare_fix(
    git_root: Option<String>,
    config: DashboardConfig,
    worktree_path: String,
    number: u64,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::FixPrPrepared(Err(
            "Could not resolve git root for the fix.".to_string(),
        )));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let event = match service.prepare_fix(&worktree_path, number).await {
            Ok(prep) => Ok(Box::new(prep)),
            Err(err) => Err(user_friendly_message(&err)),
        };
        let _ = tx.send(AppEvent::FixPrPrepared(event));
    });
}

fn kick_off_plan_comment(
    git_root: Option<String>,
    config: DashboardConfig,
    req: FixPlanRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let index = req.index;
    // The "Other" path supplies feedback; that's what makes this a re-plan that
    // must round-trip back to the Decision screen rather than auto-resolve.
    let is_replan = req.feedback.is_some();
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::FixPrPlanned {
            index,
            is_replan,
            result: Err("Could not resolve git root.".to_string()),
        });
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .plan_comment(
                &req.worktree_path,
                &req.group,
                req.feedback.as_deref(),
                req.previous_plan.as_deref(),
                req.history.as_deref(),
            )
            .await
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::FixPrPlanned {
            index,
            is_replan,
            result,
        });
    });
}

fn kick_off_prepare_apply(
    git_root: Option<String>,
    config: DashboardConfig,
    req: FixApplyRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let index = req.index;
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::FixPrApplyReady {
            index,
            result: Err("Could not resolve git root.".to_string()),
        });
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .prepare_apply(&req.worktree_path, &req.group, &req.plan)
            .await
            .map(Box::new)
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::FixPrApplyReady { index, result });
    });
}

fn kick_off_commit_and_reply(
    git_root: Option<String>,
    config: DashboardConfig,
    req: FixCommitRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let index = req.index;
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::FixPrCommitted {
            index,
            result: Err("Could not resolve git root.".to_string()),
        });
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .commit_and_reply(
                &req.worktree_path,
                &req.owner,
                &req.repo,
                req.number,
                &req.pr_url,
                req.comment_index,
                &req.group,
                &req.plan,
            )
            .await
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::FixPrCommitted { index, result });
    });
}

/// Fire-and-forget: react with 😄 to a praise comment. Errors are silently
/// dropped — the reaction is a best-effort courtesy, not part of the Fix flow.
fn kick_off_praise_reaction(
    git_root: Option<String>,
    config: DashboardConfig,
    owner: String,
    repo: String,
    worktree_path: String,
    group: CommentGroup,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let _ = service
            .react_to_praise_comment(&worktree_path, &owner, &repo, &group)
            .await;
    });
}

fn kick_off_post_reply(
    git_root: Option<String>,
    config: DashboardConfig,
    req: FixReplyRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let index = req.index;
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::FixPrReplied {
            index,
            result: Err("Could not resolve git root.".to_string()),
        });
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .post_reply(
                &req.worktree_path,
                &req.owner,
                &req.repo,
                req.number,
                &req.group,
                &req.text,
            )
            .await
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::FixPrReplied { index, result });
    });
}

fn kick_off_push_fix(
    git_root: Option<String>,
    config: DashboardConfig,
    worktree_path: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::FixPrPushed(Err(
            "Could not resolve git root.".to_string()
        )));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .push_fix(&worktree_path)
            .await
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::FixPrPushed(result));
    });
}

// ── Review Pull Request async stages ────────────────────────────────────

/// Per-file review scans kept in flight at once. Each scan is an independent
/// captured opencode call, so a small pool cuts wall-clock time without
/// hammering the provider.
const REVIEW_SCAN_CONCURRENCY: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewScanRetry {
    Initial,
    Reformat,
    Full,
}

fn next_review_retry(retry: ReviewScanRetry, has_raw_output: bool) -> Option<ReviewScanRetry> {
    match retry {
        ReviewScanRetry::Initial if has_raw_output => Some(ReviewScanRetry::Reformat),
        ReviewScanRetry::Initial | ReviewScanRetry::Reformat => Some(ReviewScanRetry::Full),
        ReviewScanRetry::Full => None,
    }
}

/// Inputs for one captured per-file scan call. `file_index` identifies the
/// file the result belongs to (scans run in parallel); `retried` marks the
/// one retry allowed after unparseable output.
struct ReviewScanRequest {
    worktree_path: String,
    group: crate::services::dashboard::ReviewFileGroup,
    context: ReviewContext,
    file_index: usize,
    retry: ReviewScanRetry,
    raw_output: Option<String>,
}

/// Inputs for the single whole-diff coverage scan. Its result comes back
/// through the same `ReviewPrScanned` event under a synthetic group index,
/// so it shares the pool's completion/retry/failure plumbing.
struct ReviewCoverageScanRequest {
    worktree_path: String,
    files: Vec<ReviewFile>,
    scan_index: usize,
    mode: ReviewScanMode,
    context: ReviewContext,
    tester_findings: Vec<ReviewFinding>,
    retry: ReviewScanRetry,
    raw_output: Option<String>,
}

/// Inputs for an "Other" revision of a single finding.
struct ReviewReviseRequest {
    worktree_path: String,
    file: ReviewFile,
    finding: ReviewFinding,
    feedback: String,
    mode: ReviewRevisionMode,
    index: usize,
}

struct ReviewVerifyRequest {
    worktree_path: String,
    file: ReviewFile,
    finding: ReviewFinding,
    context: ReviewContext,
    index: usize,
    strong: bool,
}

struct ReviewGapAuditRequest {
    worktree_path: String,
    files: Vec<ReviewFile>,
    context: ReviewContext,
    relationship_edges: String,
    skipped: Vec<crate::services::ReviewSkippedFile>,
    findings: Vec<ReviewFinding>,
}

/// Inputs for posting one approved finding on the PR.
struct ReviewPostRequest {
    worktree_path: String,
    owner: String,
    repo: String,
    number: u64,
    head_sha: String,
    finding: ReviewFinding,
    index: usize,
}

fn kick_off_prepare_review(
    git_root: Option<String>,
    config: DashboardConfig,
    worktree_path: String,
    number: u64,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::ReviewPrPrepared(Err(
            "Could not resolve git root for the review.".to_string(),
        )));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let event = match service.prepare_review(&worktree_path, number).await {
            Ok(prep) => Ok(Box::new(prep)),
            Err(err) => Err(user_friendly_message(&err)),
        };
        let _ = tx.send(AppEvent::ReviewPrPrepared(event));
    });
}

fn kick_off_scan_review_file(
    git_root: Option<String>,
    config: DashboardConfig,
    req: ReviewScanRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let file_index = req.file_index;
    let retry = req.retry;
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::ReviewPrScanned {
            file_index,
            retry,
            result: Err("Could not resolve git root.".to_string()),
            telemetry: None,
            raw_output: None,
        });
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let attempt = match retry {
            ReviewScanRetry::Reformat => {
                service
                    .reformat_review_group_output(
                        &req.worktree_path,
                        &req.group,
                        req.raw_output.as_deref().unwrap_or_default(),
                    )
                    .await
            }
            ReviewScanRetry::Initial | ReviewScanRetry::Full => {
                service
                    .scan_review_group(&req.worktree_path, &req.group, &req.context)
                    .await
            }
        };
        let result = attempt.result.map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::ReviewPrScanned {
            file_index,
            retry,
            result,
            telemetry: Some(attempt.telemetry),
            raw_output: attempt.raw_output,
        });
    });
}

fn kick_off_scan_review_coverage(
    git_root: Option<String>,
    config: DashboardConfig,
    req: ReviewCoverageScanRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let retry = req.retry;
    let scan_index = req.scan_index;
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::ReviewPrScanned {
            file_index: scan_index,
            retry,
            result: Err("Could not resolve git root.".to_string()),
            telemetry: None,
            raw_output: None,
        });
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let attempt = match retry {
            ReviewScanRetry::Reformat => match req.mode {
                ReviewScanMode::Merged => {
                    service
                        .reformat_review_merged_output(
                            &req.worktree_path,
                            &req.files,
                            req.raw_output.as_deref().unwrap_or_default(),
                        )
                        .await
                }
                ReviewScanMode::Split => {
                    service
                        .reformat_review_coverage_output(
                            &req.worktree_path,
                            &req.files,
                            req.raw_output.as_deref().unwrap_or_default(),
                        )
                        .await
                }
            },
            ReviewScanRetry::Initial | ReviewScanRetry::Full => match req.mode {
                ReviewScanMode::Merged => {
                    service
                        .scan_review_merged(
                            &req.worktree_path,
                            &req.files,
                            &req.context,
                            &req.tester_findings,
                        )
                        .await
                }
                ReviewScanMode::Split => {
                    service
                        .scan_review_coverage(
                            &req.worktree_path,
                            &req.files,
                            &req.context,
                            &req.tester_findings,
                        )
                        .await
                }
            },
        };
        let result = attempt.result.map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::ReviewPrScanned {
            file_index: scan_index,
            retry,
            result,
            telemetry: Some(attempt.telemetry),
            raw_output: attempt.raw_output,
        });
    });
}

fn kick_off_revise_review_finding(
    git_root: Option<String>,
    config: DashboardConfig,
    req: ReviewReviseRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let index = req.index;
    let mode = req.mode;
    let feedback = req.feedback.clone();
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::ReviewPrRevised {
            index,
            mode,
            feedback,
            result: Err("Could not resolve git root.".to_string()),
            telemetry: None,
        });
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let attempt = service
            .revise_review_finding(
                &req.worktree_path,
                &req.file,
                &req.finding,
                &req.feedback,
                req.mode,
            )
            .await;
        let result = attempt.result.map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::ReviewPrRevised {
            index,
            mode,
            feedback,
            result,
            telemetry: Some(attempt.telemetry),
        });
    });
}

fn kick_off_verify_review_finding(
    git_root: Option<String>,
    config: DashboardConfig,
    req: ReviewVerifyRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let index = req.index;
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::ReviewPrVerified {
            index,
            result: Err("Could not resolve git root.".to_string()),
            telemetry: None,
        });
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let attempt = service
            .verify_review_finding(
                &req.worktree_path,
                &req.file,
                &req.finding,
                &req.context,
                req.strong,
            )
            .await;
        let result = attempt.result.map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::ReviewPrVerified {
            index,
            result,
            telemetry: Some(attempt.telemetry),
        });
    });
}

fn kick_off_review_gap_audit(
    git_root: Option<String>,
    config: DashboardConfig,
    req: ReviewGapAuditRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::ReviewPrGapAudited {
            result: Err("Could not resolve git root.".to_string()),
            telemetry: None,
        });
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let attempt = service
            .scan_review_gap_audit(
                &req.worktree_path,
                &req.files,
                &req.context,
                &req.relationship_edges,
                &req.skipped,
                &req.findings,
            )
            .await;
        let result = attempt.result.map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::ReviewPrGapAudited {
            result,
            telemetry: Some(attempt.telemetry),
        });
    });
}

fn kick_off_post_review_finding(
    git_root: Option<String>,
    config: DashboardConfig,
    req: ReviewPostRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let index = req.index;
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::ReviewPrPosted {
            index,
            result: Err("Could not resolve git root.".to_string()),
        });
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .post_review_finding(
                &req.worktree_path,
                &req.owner,
                &req.repo,
                req.number,
                &req.head_sha,
                &req.finding,
            )
            .await
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::ReviewPrPosted { index, result });
    });
}

fn kick_off_submit_review_summary(
    git_root: Option<String>,
    config: DashboardConfig,
    worktree_path: String,
    number: u64,
    body: String,
    request_changes: bool,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::ReviewPrSummarySubmitted {
            request_changes,
            result: Err("Could not resolve git root.".to_string()),
        });
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .submit_review_summary(&worktree_path, number, &body, request_changes)
            .await
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::ReviewPrSummarySubmitted {
            request_changes,
            result,
        });
    });
}

// ── Bugkill async stages ────────────────────────────────────────────────

/// Inputs for one Bugkill fix attempt (snapshot + opencode spawn params).
struct BugkillPrepareFixRequest {
    worktree_path: String,
    bug_description: String,
    row: BugHypothesis,
    row_index: usize,
    feedback: Option<String>,
}

/// Inputs for the post-attempt scan + harness commit.
struct BugkillCommitRequest {
    worktree_path: String,
    pre: BugkillSnapshot,
    number: usize,
    solution: String,
    amend: bool,
}

fn kick_off_bugkill_preflight(
    git_root: Option<String>,
    config: DashboardConfig,
    worktree_path: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::BugkillPrepared(Err(
            "Could not resolve git root.".to_string(),
        )));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .bugkill_preflight(&worktree_path)
            .await
            .map(Box::new)
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::BugkillPrepared(result));
    });
}

fn kick_off_bugkill_discard(
    git_root: Option<String>,
    config: DashboardConfig,
    worktree_path: String,
    paths: Vec<String>,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::BugkillDiscarded(Err(
            "Could not resolve git root.".to_string(),
        )));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .bugkill_abort_cleanup(&worktree_path, &paths)
            .await
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::BugkillDiscarded(result));
    });
}

fn kick_off_bugkill_prepare_investigate(
    git_root: Option<String>,
    config: DashboardConfig,
    worktree_path: String,
    bug_description: String,
    base_ref: Option<String>,
    corrective: bool,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::BugkillInvestigateReady {
            corrective,
            result: Err("Could not resolve git root.".to_string()),
        });
        return;
    };
    // A task despite the call being synchronous: the opencode-on-PATH gate
    // inside spawns `opencode --version`, which must not block the UI.
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .prepare_bugkill_investigate(
                &worktree_path,
                &bug_description,
                base_ref.as_deref(),
                corrective,
            )
            .map(Box::new)
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::BugkillInvestigateReady { corrective, result });
    });
}

fn kick_off_bugkill_prepare_fix(
    git_root: Option<String>,
    config: DashboardConfig,
    req: BugkillPrepareFixRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let row_index = req.row_index;
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::BugkillFixReady {
            row_index,
            result: Err("Could not resolve git root.".to_string()),
        });
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        // Snapshot first (the pre-attempt baseline), then the spawn params.
        let result = async {
            let snapshot = service.bugkill_snapshot(&req.worktree_path).await?;
            let handoff = service
                .prepare_bugkill_fix(
                    &req.worktree_path,
                    &req.bug_description,
                    &req.row,
                    req.feedback.as_deref(),
                )
                .await?;
            Ok::<_, crate::errors::WisetreeError>(Box::new((snapshot, handoff)))
        }
        .await
        .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::BugkillFixReady { row_index, result });
    });
}

fn kick_off_bugkill_commit(
    git_root: Option<String>,
    config: DashboardConfig,
    req: BugkillCommitRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::BugkillCommitted(Err(
            "Could not resolve git root.".to_string(),
        )));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = async {
            let post = service.bugkill_snapshot(&req.worktree_path).await?;
            let changes =
                compute_attempt_changes(&post.tracked, &post.untracked, &req.pre.untracked);
            // Only tracked/committable paths make an attempt: a change-set
            // that is empty (or holds only modified pre-existing untracked
            // files) cannot be committed or reverted — the row stays
            // eligible and the user is told the AI made no changes.
            if changes.commit_paths.is_empty() {
                return Ok(BugkillCommitOutcome::NoChanges);
            }
            let sha = service
                .bugkill_commit_attempt(
                    &req.worktree_path,
                    &changes,
                    req.number,
                    &req.solution,
                    req.amend,
                )
                .await?;
            Ok::<_, crate::errors::WisetreeError>(BugkillCommitOutcome::Committed { sha, changes })
        }
        .await
        .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::BugkillCommitted(result));
    });
}

fn kick_off_bugkill_abort(
    git_root: Option<String>,
    config: DashboardConfig,
    worktree_path: String,
    pre: BugkillSnapshot,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::BugkillAborted(Err(
            "Could not resolve git root.".to_string()
        )));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = async {
            let post = service.bugkill_snapshot(&worktree_path).await?;
            let changes = compute_attempt_changes(&post.tracked, &post.untracked, &pre.untracked);
            service
                .bugkill_abort_cleanup(&worktree_path, &changes.all)
                .await
        }
        .await
        .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::BugkillAborted(result));
    });
}

fn kick_off_bugkill_judge(
    git_root: Option<String>,
    config: DashboardConfig,
    worktree_path: String,
    row: BugHypothesis,
    user_text: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::BugkillJudged {
            user_text,
            result: Err("Could not resolve git root.".to_string()),
        });
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .bugkill_judge(&worktree_path, &row, &user_text)
            .await
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::BugkillJudged { user_text, result });
    });
}

fn kick_off_bugkill_rollback(
    git_root: Option<String>,
    config: DashboardConfig,
    worktree_path: String,
    sha: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::BugkillRolledBack(Err(
            "Could not resolve git root.".to_string(),
        )));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .bugkill_rollback(&worktree_path, &sha)
            .await
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::BugkillRolledBack(result));
    });
}

fn kick_off_update_branch(
    config: DashboardConfig,
    worktree_path: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        // `update_branch` runs every git command with the worktree path as
        // its cwd, so the service root is only a placeholder here — reuse
        // the worktree path rather than resolving a separate "git_root".
        // Works for any worktree, mother or derived.
        let service = DashboardService::new(PathBuf::from(&worktree_path), config);
        let event = match service.update_branch(&worktree_path).await {
            Ok(outcome) => Ok(outcome),
            Err(err) => Err(user_friendly_message(&err)),
        };
        let _ = tx.send(AppEvent::UpdateBranchFinished(event));
    });
}

fn kick_off_abort_ai_merge(
    git_root: Option<String>,
    config: DashboardConfig,
    request: UpdatePullRequestRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let number = request.number;
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::UpdatePrFinished(Err(UpdatePrFailure {
            number,
            message: "Could not resolve git root for abort.".to_string(),
        })));
        return;
    };
    let base_ref = request
        .base_ref
        .clone()
        .unwrap_or_else(|| "(unknown)".to_string());
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service.abort_ai_merge(&request.worktree_path).await;
        let event = match result {
            Ok(outcome) => Ok(UpdatePrSuccess {
                number,
                base_ref,
                outcome,
            }),
            Err(err) => Err(UpdatePrFailure {
                number,
                message: user_friendly_message(&err),
            }),
        };
        let _ = tx.send(AppEvent::UpdatePrFinished(event));
    });
}

fn kick_off_update_pull_request(
    git_root: Option<String>,
    config: DashboardConfig,
    request: UpdatePullRequestRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let number = request.number;
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::UpdatePrFinished(Err(UpdatePrFailure {
            number,
            message: "Could not resolve git root for update.".to_string(),
        })));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        // The single-worktree flow resolves `base_ref` before confirming, so
        // it is `Some` here. The "Update all" batch skips the Confirm step and
        // passes `None`, so resolve it inline (falls back to a failure event
        // if nothing in `BASE_REF_PRIORITY` is reachable).
        let base_ref = match request.base_ref.clone() {
            Some(base_ref) => base_ref,
            None => {
                match crate::services::dashboard::resolve_base_ref(
                    &PathBuf::from(&request.worktree_path),
                    request.pr_base_ref.as_deref(),
                )
                .await
                {
                    Some(base_ref) => base_ref,
                    None => {
                        let _ = tx.send(AppEvent::UpdatePrFinished(Err(UpdatePrFailure {
                            number,
                            message: "No base ref reachable (looked for upstream/main, \
                                      upstream/master, origin/main, origin/master)."
                                .to_string(),
                        })));
                        return;
                    }
                }
            }
        };

        // Bridge: pipe `UpdateProgress` events from the service into the
        // App's `AppEvent` channel so phase toasts and AI output land on
        // the same event loop as everything else.
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<UpdateProgress>();
        let forward_tx = tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(progress) = progress_rx.recv().await {
                if forward_tx
                    .send(AppEvent::UpdatePrProgress { number, progress })
                    .is_err()
                {
                    break;
                }
            }
        });

        let result = service
            .update_pull_request_with_progress(
                &request.worktree_path,
                &base_ref,
                request.autonomous,
                Some(progress_tx),
            )
            .await;
        // Drop the progress sender (the service already did, but be
        // explicit) and wait for the forwarder to drain before emitting
        // the terminal event so the activity panel never lags behind.
        let _ = forwarder.await;

        let event = match result {
            Ok(outcome) => Ok(UpdatePrSuccess {
                number,
                base_ref,
                outcome,
            }),
            Err(err) => Err(UpdatePrFailure {
                number,
                message: user_friendly_message(&err),
            }),
        };
        let _ = tx.send(AppEvent::UpdatePrFinished(event));
    });
}

/// Push-only counterpart to `kick_off_update_pull_request`: runs
/// `git push origin HEAD` against the worktree and reports `Pushed` /
/// `PushFailed`. Powers both the dashboard's "Push Pull Request" action and
/// the Terminal Activity panel's "Accept" re-push. A `PushFailed` result is
/// handled by `apply_update_pr_finished`, which re-opens the recovery panel.
fn kick_off_push_pull_request(
    git_root: Option<String>,
    config: DashboardConfig,
    request: UpdatePullRequestRequest,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let number = request.number;
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::UpdatePrFinished(Err(UpdatePrFailure {
            number,
            message: "Could not resolve git root for push.".to_string(),
        })));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .push_pull_request_with_progress(&request.worktree_path, None)
            .await;
        let event = match result {
            Ok(outcome) => Ok(UpdatePrSuccess {
                number,
                // A push has no base ref; the `Pushed` toast doesn't use it.
                base_ref: String::new(),
                outcome,
            }),
            Err(err) => Err(UpdatePrFailure {
                number,
                message: user_friendly_message(&err),
            }),
        };
        let _ = tx.send(AppEvent::UpdatePrFinished(event));
    });
}

/// Resolve the user's interactive login shell plus the args that make it
/// source their profile (`~/.bash_profile`, `~/.zprofile`, …) — i.e. behave
/// like a freshly opened terminal. Prefers `$SHELL` (the user's actual login
/// shell), falling back to common shells. Shared by every embedded inner
/// terminal so they all start from the same environment.
fn login_shell() -> (PathBuf, Vec<String>) {
    let shell = std::env::var("SHELL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            ["/bin/zsh", "/bin/bash", "/bin/sh"]
                .into_iter()
                .map(PathBuf::from)
                .find(|p| p.exists())
        })
        .unwrap_or_else(|| PathBuf::from("/bin/sh"));
    let args = shell_login_args(&shell);
    (shell, args)
}

/// Wrap `program` + `args` so they run *inside* the user's login shell — so a
/// non-shell inner terminal (e.g. the opencode conflict resolver) still starts
/// from a profile-sourced environment (PATH, env vars, functions), exactly as
/// if the user had launched it from a freshly opened terminal.
///
/// Uses the `exec "$@"` idiom: the wrapped argv is handed to the shell as
/// positional parameters and expanded verbatim, never re-parsed — so an
/// AI-merge prompt containing backticks, `$(...)`, quotes, etc. can't be
/// interpreted by the shell (no quoting pitfalls, no injection surface).
fn login_shell_command(program: &std::path::Path, args: &[String]) -> (PathBuf, Vec<String>) {
    let (shell, mut shell_args) = login_shell();
    shell_args.push("-c".to_string());
    shell_args.push("exec \"$@\"".to_string());
    // $0 — a conventional label for the execed process.
    shell_args.push(
        shell
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("sh")
            .to_string(),
    );
    // $1.. — the real program and its args, passed through untouched.
    shell_args.push(program.to_string_lossy().into_owned());
    shell_args.extend(args.iter().cloned());
    (shell, shell_args)
}

/// Build login + interactive args so the recovery shell sources the user's
/// profile (`~/.bash_profile`, `~/.zprofile`, `~/.zshrc`, …) — making their
/// custom functions and aliases (e.g. an `update()` defined in
/// `~/.bash_profile`) available, exactly as a freshly opened terminal would.
/// `-l` (login) is what pulls in the profile; `-i` forces interactive mode.
/// This keys off the shell's name rather than the OS, so it works wherever
/// the user's `$SHELL` points. POSIX `sh`/`dash` reject `-l`, so they only
/// receive `-i`.
fn shell_login_args(shell: &std::path::Path) -> Vec<String> {
    let name = shell
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        // Some environments report `$SHELL` (or argv0) as e.g. `-bash`.
        .trim_start_matches('-');
    match name {
        "bash" | "zsh" | "fish" | "ksh" | "ksh93" | "mksh" | "tcsh" | "csh" => {
            vec!["-l".to_string(), "-i".to_string()]
        }
        // sh / dash and anything unrecognized: interactive only (no `-l`,
        // which dash treats as an illegal option).
        _ => vec!["-i".to_string()],
    }
}

fn kick_off_setup_install(shell: Shell, tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            install_shell_integration(shell, "wisetree")
                .map(|_| detect_shell_integration())
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|result| result);

        let _ = tx.send(AppEvent::SetupInstalled(result));
    });
}

fn screen_delete_outcome(outcome: ServiceDeleteOutcome) -> ScreenDeleteOutcome {
    ScreenDeleteOutcome {
        worktree_deleted: outcome.worktree_deleted,
        branch_deleted: outcome.branch_deleted,
        branch_name: outcome.branch_name,
    }
}

/// Flatten the create outcome into one row per executed action so the
/// success screen can render a status table (Command | Status | Failure).
fn create_summary_rows(outcome: &ServiceCreateOutcome) -> Vec<SummaryRow> {
    let mut rows: Vec<SummaryRow> = Vec::new();

    if let Some(report) = &outcome.copy_report {
        let label = format!("Copy patterns ({} copied)", report.copied.len());
        if report.errors.is_empty() {
            rows.push(SummaryRow::success(label));
        } else {
            rows.push(SummaryRow::failure(label, report.errors.join("; ")));
        }

        if !report.skipped.is_empty() {
            rows.push(SummaryRow::success(format!(
                "Ignore patterns ({} skipped)",
                report.skipped.len()
            )));
        }
    }

    if let Some(report) = &outcome.link_report {
        let label = format!("Link patterns ({} linked)", report.linked.len());
        if report.errors.is_empty() {
            rows.push(SummaryRow::success(label));
        } else {
            rows.push(SummaryRow::failure(label, report.errors.join("; ")));
        }
    }

    for run in &outcome.command_runs {
        if run.success {
            rows.push(SummaryRow::success(run.command.clone()));
        } else {
            // Prefer the explicit error string; otherwise fall back to the
            // last non-empty line of captured output so the user still sees
            // *something* concrete in the Failure column.
            let reason = run
                .error
                .clone()
                .or_else(|| {
                    run.output
                        .lines()
                        .map(|line| line.trim())
                        .rev()
                        .find(|line| !line.is_empty())
                        .map(|line| line.to_string())
                })
                .unwrap_or_else(|| "Command failed".to_string());
            rows.push(SummaryRow::failure(run.command.clone(), reason));
        }
    }

    rows
}

fn summarize_wise_preset_matches(discovery: &WisePresetDiscovery) -> String {
    let labels: Vec<&'static str> = discovery
        .matched_ids
        .iter()
        .filter_map(|id| crate::services::presets::find_by_id(*id).map(|preset| preset.label))
        .collect();

    match labels.as_slice() {
        [] => "no matches".to_string(),
        [only] => only.to_string(),
        [first, second] => format!("{first} and {second}"),
        [first, second, third] => format!("{first}, {second}, and {third}"),
        [first, second, third, rest @ ..] => {
            format!("{first}, {second}, {third}, and {} more", rest.len())
        }
    }
}

fn copy_to_clipboard(value: &str) -> std::result::Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use std::io::Write;

        let mut child = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| err.to_string())?;
        let Some(mut stdin) = child.stdin.take() else {
            return Err("clipboard stdin unavailable".to_string());
        };
        stdin
            .write_all(value.as_bytes())
            .map_err(|err| err.to_string())?;
        // Drop stdin to signal EOF — pbcopy reads until the pipe closes and
        // child.wait() would otherwise deadlock the UI thread.
        drop(stdin);
        let status = child.wait().map_err(|err| err.to_string())?;
        return if status.success() {
            Ok(())
        } else {
            Err("pbcopy exited unsuccessfully".to_string())
        };
    }

    #[cfg(target_os = "linux")]
    {
        use std::io::Write;

        for program in ["wl-copy", "xclip"] {
            let mut command = std::process::Command::new(program);
            if program == "xclip" {
                command.args(["-selection", "clipboard"]);
            }
            match command.stdin(std::process::Stdio::piped()).spawn() {
                Ok(mut child) => {
                    let Some(mut stdin) = child.stdin.take() else {
                        continue;
                    };
                    let _ = stdin.write_all(value.as_bytes());
                    drop(stdin);
                    if child.wait().map(|status| status.success()).unwrap_or(false) {
                        return Ok(());
                    }
                }
                Err(_) => continue,
            }
        }
        return Err("no supported clipboard tool found".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        use std::io::Write;

        let mut child = std::process::Command::new("clip")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| err.to_string())?;
        let Some(mut stdin) = child.stdin.take() else {
            return Err("clipboard stdin unavailable".to_string());
        };
        stdin
            .write_all(value.as_bytes())
            .map_err(|err| err.to_string())?;
        drop(stdin);
        let status = child.wait().map_err(|err| err.to_string())?;
        return if status.success() {
            Ok(())
        } else {
            Err("clip exited unsuccessfully".to_string())
        };
    }

    #[allow(unreachable_code)]
    Err("clipboard is unavailable on this platform".to_string())
}

fn fold_path(path: &str) -> String {
    crate::tui::widgets::welcome_header::fold_home(path)
}

/// Cap a captured stderr/stdout snippet to a single readable line so it
/// fits in a toast. Joins all lines on a single space and adds an ellipsis
/// when the text exceeds the limit.
fn truncate_error(text: &str) -> String {
    let compact = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = compact.trim();
    let limit = 160;
    if trimmed.chars().count() > limit {
        let truncated: String = trimmed.chars().take(limit).collect();
        format!("{truncated}…")
    } else {
        trimmed.to_string()
    }
}

fn clipboard_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        return command_in_path("pbcopy");
    }

    #[cfg(target_os = "linux")]
    {
        return command_in_path("wl-copy") || command_in_path("xclip");
    }

    #[cfg(target_os = "windows")]
    {
        return command_in_path("clip") || command_in_path("clip.exe");
    }

    #[allow(unreachable_code)]
    false
}

fn command_in_path(program: &str) -> bool {
    let Some(path_var) = env::var_os("PATH") else {
        return false;
    };
    let candidates = candidate_program_names(program);

    env::split_paths(&path_var).any(|directory| {
        candidates
            .iter()
            .map(|name| directory.join(name))
            .any(|candidate| candidate.is_file())
    })
}

fn candidate_program_names(program: &str) -> Vec<OsString> {
    #[cfg(not(target_os = "windows"))]
    let candidates = vec![OsString::from(program)];

    #[cfg(target_os = "windows")]
    let mut candidates = vec![OsString::from(program)];

    #[cfg(target_os = "windows")]
    {
        if !program.contains('.') {
            candidates.push(OsString::from(format!("{program}.exe")));
            candidates.push(OsString::from(format!("{program}.cmd")));
            candidates.push(OsString::from(format!("{program}.bat")));
        }
    }

    candidates
}

fn reset_global_config() -> Result<(), String> {
    let mut svc = ConfigService::new();
    svc.create_global_config()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::WorktreeConfig;
    use crate::config::service::ConfigService;
    use crate::services::{AiStatusReport, PullRequest, ReviewerSummary};
    use crossterm::event::{KeyEventKind, KeyEventState};
    use once_cell::sync::Lazy;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::fs;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static HOME_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn app_event_tx() -> mpsc::UnboundedSender<AppEvent> {
        let (tx, _rx) = mpsc::unbounded_channel();
        tx
    }

    #[test]
    fn review_retry_routes_parse_failures_through_reformat_then_full_scan() {
        assert_eq!(
            next_review_retry(ReviewScanRetry::Initial, true),
            Some(ReviewScanRetry::Reformat)
        );
        assert_eq!(
            next_review_retry(ReviewScanRetry::Reformat, true),
            Some(ReviewScanRetry::Full)
        );
        assert_eq!(next_review_retry(ReviewScanRetry::Full, true), None);
        // A spawn/timeout failure has no raw text to reformat, so preserve the
        // former behavior: go straight to the one full retry.
        assert_eq!(
            next_review_retry(ReviewScanRetry::Initial, false),
            Some(ReviewScanRetry::Full)
        );
    }

    fn review_scan_test_app(mode: ReviewScanMode, paths: &[&str]) -> App {
        let request = ReviewPullRequestRequest {
            number: 42,
            title: "Review retries".to_string(),
            url: "https://example.test/pull/42".to_string(),
            branch: "review-retries".to_string(),
            worktree_path: "/tmp/review-retries".to_string(),
        };
        let model = crate::config::schema::AiModelConfig {
            model: "opencode/test".to_string(),
            thinking: "max".to_string(),
        };
        let ai = crate::config::schema::AiReviewConfig {
            strong: model.clone(),
            balanced: model.clone(),
            utility: model,
        };
        let files = paths
            .iter()
            .map(|path| ReviewFile {
                path: (*path).to_string(),
                annotated_diff: "@@ -1 +1 @@\n     1 +changed".to_string(),
                full_content: None,
                commentable_lines: std::collections::BTreeSet::from([1]),
                existing_comments: String::new(),
                existing_keys: Vec::new(),
            })
            .collect();
        let mut screen = ReviewPullRequestScreen::new(request, ai);
        screen.set_scan_mode(mode);
        screen.set_files(files, "o".into(), "r".into(), "sha".into());
        screen.begin_scan_phase();

        let mut app = App::new(AppMode::Dashboard, false);
        app.screen = Screen::ReviewPullRequest;
        app.review_pr = Some(screen);
        app
    }

    fn review_test_telemetry(scan: &str) -> ReviewScanTelemetry {
        ReviewScanTelemetry {
            scan: scan.to_string(),
            scan_role: "test".to_string(),
            retry_role: "initial".to_string(),
            model_profile: "balanced".to_string(),
            model: "openai/gpt-5.6-terra".to_string(),
            thinking: "medium".to_string(),
            prompt_bytes: 100,
            usage: crate::services::ReviewTokenUsage {
                uncached_input: Some(10),
                cache_read: Some(0),
                cache_write: Some(0),
                output: Some(2),
                reasoning: Some(0),
                cost_usd: None,
            },
            duration_ms: 5,
            findings: 0,
        }
    }

    #[test]
    fn review_file_reformat_success_settles_the_scan() {
        let mut app = review_scan_test_app(ReviewScanMode::Split, &["tests/unit_test.rs"]);
        app.review_pr.as_mut().unwrap().take_next_scan_file();
        let tx = app_event_tx();
        app.apply_review_pr_scanned(
            0,
            ReviewScanRetry::Initial,
            Err("malformed".to_string()),
            None,
            Some("bad output".to_string()),
            &tx,
        );
        assert!(app.review_pr.as_ref().unwrap().scan_phase_active());
        app.apply_review_pr_scanned(
            0,
            ReviewScanRetry::Reformat,
            Ok(Vec::new()),
            None,
            None,
            &tx,
        );
        assert_eq!(
            app.review_pr.as_ref().unwrap().step(),
            crate::tui::screens::review_pr::ReviewStep::Done
        );
        app.apply_review_pr_scanned(
            0,
            ReviewScanRetry::Full,
            Ok(Vec::new()),
            Some(ReviewScanTelemetry {
                scan: "late:test".to_string(),
                scan_role: "test".to_string(),
                retry_role: "full-rescan".to_string(),
                model_profile: "balanced".to_string(),
                model: "openai/gpt-5.6-terra".to_string(),
                thinking: "medium".to_string(),
                prompt_bytes: 1,
                usage: crate::services::ReviewTokenUsage {
                    uncached_input: Some(1),
                    cache_read: Some(0),
                    cache_write: Some(0),
                    output: Some(1),
                    reasoning: Some(0),
                    cost_usd: None,
                },
                duration_ms: 1,
                findings: 0,
            }),
            None,
            &tx,
        );
        assert_eq!(app.review_pr.as_ref().unwrap().scan_telemetry_len(), 0);
    }

    #[test]
    fn review_multi_file_tester_group_retries_and_settles_once() {
        let mut app = review_scan_test_app(
            ReviewScanMode::Split,
            &["tests/user_test.rs", "tests/user_spec.rs"],
        );
        let group_index = app
            .review_pr
            .as_mut()
            .unwrap()
            .take_next_scan_file()
            .unwrap()
            .0;
        let tx = app_event_tx();
        app.apply_review_pr_scanned(
            group_index,
            ReviewScanRetry::Initial,
            Err("malformed".to_string()),
            None,
            Some("bad output".to_string()),
            &tx,
        );
        app.apply_review_pr_scanned(
            group_index,
            ReviewScanRetry::Reformat,
            Ok(Vec::new()),
            None,
            None,
            &tx,
        );
        assert_eq!(
            app.review_pr.as_ref().unwrap().step(),
            crate::tui::screens::review_pr::ReviewStep::Done
        );
    }

    #[test]
    fn failed_focused_revision_dispatches_one_expanded_retry() {
        let mut app = review_scan_test_app(ReviewScanMode::Split, &["src/lib.rs"]);
        let screen = app.review_pr.as_mut().unwrap();
        screen.record_scan_result(vec![ReviewFinding {
            category: "Code Smell".to_string(),
            severity: crate::services::ReviewSeverity::Low,
            file: "src/lib.rs".to_string(),
            start_line: None,
            line: Some(1),
            title: "Unclear branch".to_string(),
            explanation: "The branch is hard to follow.".to_string(),
            suggestion: None,
        }]);
        screen.finish_scanning();
        screen.enter_decision();
        screen.start_revising();
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.apply_review_pr_revised(
            0,
            ReviewRevisionMode::Focused,
            "use more context".to_string(),
            Err("malformed".to_string()),
            None,
            &tx,
        );
        let event = rx.try_recv().expect("expanded retry event");
        assert!(matches!(
            event,
            AppEvent::ReviewPrRevised {
                mode: ReviewRevisionMode::Expanded,
                ..
            }
        ));
    }

    #[test]
    fn review_coverage_reformat_failure_falls_back_to_one_full_scan() {
        let mut app = review_scan_test_app(ReviewScanMode::Split, &["src/lib.rs"]);
        let screen = app.review_pr.as_mut().unwrap();
        screen.take_next_scan_file();
        screen.take_coverage_scan();
        let tx = app_event_tx();
        app.apply_review_pr_scanned(0, ReviewScanRetry::Initial, Ok(Vec::new()), None, None, &tx);
        app.apply_review_pr_scanned(
            COVERAGE_SCAN_INDEX,
            ReviewScanRetry::Initial,
            Err("malformed".to_string()),
            Some(review_test_telemetry("coverage")),
            Some("bad output".to_string()),
            &tx,
        );
        app.apply_review_pr_scanned(
            COVERAGE_SCAN_INDEX,
            ReviewScanRetry::Reformat,
            Err("still malformed".to_string()),
            Some(review_test_telemetry("reformat:coverage")),
            Some("still bad".to_string()),
            &tx,
        );
        app.apply_review_pr_scanned(
            COVERAGE_SCAN_INDEX,
            ReviewScanRetry::Full,
            Ok(Vec::new()),
            Some(review_test_telemetry("coverage")),
            None,
            &tx,
        );
        assert_eq!(
            app.review_pr.as_ref().unwrap().step(),
            crate::tui::screens::review_pr::ReviewStep::Done
        );
        assert_eq!(app.review_pr.as_ref().unwrap().scan_telemetry_len(), 3);
    }

    #[test]
    fn review_retry_terminal_failures_settle_file_and_merged_scans() {
        for (mode, path, index) in [
            (ReviewScanMode::Split, "tests/unit_test.rs", 0),
            (ReviewScanMode::Merged, "src/lib.rs", COVERAGE_SCAN_INDEX),
        ] {
            let mut app = review_scan_test_app(mode, &[path]);
            let screen = app.review_pr.as_mut().unwrap();
            if index == COVERAGE_SCAN_INDEX {
                screen.take_coverage_scan();
            } else {
                screen.take_next_scan_file();
            }
            let tx = app_event_tx();
            for (retry, raw_output) in [
                (ReviewScanRetry::Initial, Some("bad output".to_string())),
                (
                    ReviewScanRetry::Reformat,
                    Some("still malformed".to_string()),
                ),
                (ReviewScanRetry::Full, None),
            ] {
                app.apply_review_pr_scanned(
                    index,
                    retry,
                    Err("malformed".to_string()),
                    None,
                    raw_output,
                    &tx,
                );
            }
            assert_eq!(
                app.review_pr.as_ref().unwrap().step(),
                crate::tui::screens::review_pr::ReviewStep::Done
            );
        }
    }

    fn notification_config(ai_status_ok: bool, pr_checks_ok: bool) -> NotificationsConfig {
        NotificationsConfig {
            ai_status_ok,
            pr_checks_ok,
        }
    }

    fn ai_report(status: AiStatus) -> AiStatusReport {
        AiStatusReport {
            aggregated: status,
            per_harness: Default::default(),
        }
    }

    fn pr(number: u64, checks_status: Option<CheckStatus>) -> PullRequest {
        PullRequest {
            number,
            state: PrState::Open,
            url: format!("https://example.test/pull/{number}"),
            title: format!("PR {number}"),
            base_ref_name: None,
            base_repository: None,
            head_ref_oid: None,
            labels: Vec::new(),
            checks_status,
            review_status: None,
            merge_status: None,
            reviewers: ReviewerSummary::default(),
        }
    }

    fn dashboard_row(
        path: &str,
        branch: &str,
        ai_status: Option<AiStatus>,
        pull_request: Option<PullRequest>,
    ) -> DashboardRow {
        DashboardRow {
            worktree: GitWorktree {
                path: path.into(),
                branch: branch.into(),
                commit: "deadbeef".into(),
                is_main: false,
                is_clean: true,
                branch_status: None,
            },
            last_commit: None,
            pull_request,
            ai_status: ai_status.map(ai_report),
            error: None,
        }
    }

    fn git_update(rows: Vec<DashboardRow>) -> DashboardUpdate {
        DashboardUpdate::GitOnly(rows)
    }

    fn pr_update(rows: Vec<DashboardRow>) -> DashboardUpdate {
        DashboardUpdate::WithPRs {
            rows,
            next_pr_fetch_at: None,
        }
    }

    #[test]
    fn dashboard_notifications_do_not_ring_on_initial_ok_states() {
        let config = notification_config(true, true);
        let mut snapshot = None;
        let update = pr_update(vec![dashboard_row(
            "/repo/feature",
            "feature",
            Some(AiStatus::Finished),
            Some(pr(42, Some(CheckStatus::Passed))),
        )]);

        assert!(!dashboard_update_requests_bell(
            &mut snapshot,
            &update,
            &config
        ));
    }

    #[test]
    fn dashboard_notifications_ai_transition_respects_setting() {
        let enabled = notification_config(true, false);
        let disabled = notification_config(false, false);
        let initial = git_update(vec![dashboard_row(
            "/repo/feature",
            "feature",
            Some(AiStatus::InProgress),
            None,
        )]);
        let finished = git_update(vec![dashboard_row(
            "/repo/feature",
            "feature",
            Some(AiStatus::Finished),
            None,
        )]);

        let mut snapshot = None;
        assert!(!dashboard_update_requests_bell(
            &mut snapshot,
            &initial,
            &enabled
        ));
        assert!(dashboard_update_requests_bell(
            &mut snapshot,
            &finished,
            &enabled
        ));

        let mut snapshot = None;
        assert!(!dashboard_update_requests_bell(
            &mut snapshot,
            &initial,
            &disabled
        ));
        assert!(!dashboard_update_requests_bell(
            &mut snapshot,
            &finished,
            &disabled
        ));
    }

    #[test]
    fn dashboard_notifications_pr_checks_transition_respects_setting() {
        let enabled = notification_config(false, true);
        let disabled = notification_config(false, false);
        let running = pr_update(vec![dashboard_row(
            "/repo/feature",
            "feature",
            None,
            Some(pr(42, Some(CheckStatus::Running))),
        )]);
        let passed = pr_update(vec![dashboard_row(
            "/repo/feature",
            "feature",
            None,
            Some(pr(42, Some(CheckStatus::Passed))),
        )]);

        let mut snapshot = None;
        assert!(!dashboard_update_requests_bell(
            &mut snapshot,
            &running,
            &enabled
        ));
        assert!(dashboard_update_requests_bell(
            &mut snapshot,
            &passed,
            &enabled
        ));

        let mut snapshot = None;
        assert!(!dashboard_update_requests_bell(
            &mut snapshot,
            &running,
            &disabled
        ));
        assert!(!dashboard_update_requests_bell(
            &mut snapshot,
            &passed,
            &disabled
        ));
    }

    #[test]
    fn dashboard_notifications_ignore_missing_values() {
        let config = notification_config(true, true);
        let mut snapshot = None;
        let active = pr_update(vec![dashboard_row(
            "/repo/feature",
            "feature",
            Some(AiStatus::InProgress),
            Some(pr(42, Some(CheckStatus::Running))),
        )]);
        let missing = pr_update(vec![dashboard_row(
            "/repo/feature",
            "feature",
            None,
            Some(pr(42, None)),
        )]);
        let ok = pr_update(vec![dashboard_row(
            "/repo/feature",
            "feature",
            Some(AiStatus::Finished),
            Some(pr(42, Some(CheckStatus::Passed))),
        )]);

        assert!(!dashboard_update_requests_bell(
            &mut snapshot,
            &active,
            &config
        ));
        assert!(!dashboard_update_requests_bell(
            &mut snapshot,
            &missing,
            &config
        ));
        assert!(!dashboard_update_requests_bell(&mut snapshot, &ok, &config));
    }

    #[test]
    fn dashboard_notifications_ignore_pr_checks_on_git_only_updates() {
        let config = notification_config(false, true);
        let mut snapshot = None;
        let running = pr_update(vec![dashboard_row(
            "/repo/feature",
            "feature",
            None,
            Some(pr(42, Some(CheckStatus::Running))),
        )]);
        let git_only_passed = git_update(vec![dashboard_row(
            "/repo/feature",
            "feature",
            None,
            Some(pr(42, Some(CheckStatus::Passed))),
        )]);
        let pr_passed = pr_update(vec![dashboard_row(
            "/repo/feature",
            "feature",
            None,
            Some(pr(42, Some(CheckStatus::Passed))),
        )]);

        assert!(!dashboard_update_requests_bell(
            &mut snapshot,
            &running,
            &config
        ));
        assert!(!dashboard_update_requests_bell(
            &mut snapshot,
            &git_only_passed,
            &config
        ));
        assert!(dashboard_update_requests_bell(
            &mut snapshot,
            &pr_passed,
            &config
        ));
    }

    #[test]
    fn shell_login_args_are_login_interactive_for_common_shells() {
        let expected: Vec<String> = vec!["-l".into(), "-i".into()];
        for path in [
            "/bin/bash",
            "/usr/bin/zsh",
            "/opt/homebrew/bin/bash",
            "/usr/local/bin/fish",
        ] {
            assert_eq!(
                shell_login_args(std::path::Path::new(path)),
                expected,
                "expected login+interactive args for {path}"
            );
        }
    }

    #[test]
    fn shell_login_args_skip_login_flag_for_posix_sh() {
        // dash rejects `-l`, so sh/dash and unknown shells get interactive only.
        let expected: Vec<String> = vec!["-i".into()];
        for path in ["/bin/sh", "/bin/dash", "/usr/bin/some-exotic-shell"] {
            assert_eq!(shell_login_args(std::path::Path::new(path)), expected);
        }
    }

    #[test]
    fn login_shell_command_uses_exec_idiom_and_passes_args_verbatim() {
        // A program + an arg containing shell metacharacters that must NOT be
        // interpreted (the AI-merge prompt can contain backticks, $(), etc.).
        let dangerous = "resolve `rm -rf /` and $(whoami)".to_string();
        let (_shell, args) = login_shell_command(
            std::path::Path::new("/usr/local/bin/opencode"),
            &["--prompt".to_string(), dangerous.clone(), "-m".to_string()],
        );
        // The shell receives `... -c 'exec "$@"' <$0> <program> <args...>`.
        let c_idx = args.iter().position(|a| a == "-c").expect("-c present");
        assert_eq!(args[c_idx + 1], "exec \"$@\"");
        // $0 is a label, then the program, then the args passed through as-is.
        assert_eq!(args[c_idx + 3], "/usr/local/bin/opencode");
        assert_eq!(args[c_idx + 4], "--prompt");
        assert_eq!(
            args[c_idx + 5],
            dangerous,
            "prompt arg must be forwarded verbatim, never re-parsed"
        );
        assert_eq!(args[c_idx + 6], "-m");
    }

    fn with_home<F: FnOnce(&TempDir)>(f: F) {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        f(&tmp);
        if let Some(p) = prev {
            std::env::set_var("HOME", p);
        } else {
            std::env::remove_var("HOME");
        }
    }

    fn write(root: &std::path::Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn initialized_menu_app() -> App {
        // A persistent tempdir with a stub `.wisetree.json` so
        // `has_local_config()` is true and the "Setup Project Config"
        // entry is hidden — keeping menu ordering stable for these tests.
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.keep();
        fs::write(repo_root.join(LOCAL_CONFIG_FILE_NAME), "{}").expect("write local config");

        let service = WorktreeService::new(None);

        let mut app = App::new(AppMode::Menu, false);
        app.phase = InitPhase::Ready;
        app.worktree_service = Some(service);
        app.git_root = Some(repo_root.display().to_string());
        app.shell_integration_status = Some(ShellIntegrationStatus {
            is_installed: true,
            shell: Shell::Zsh,
            config_path: None,
            reason: None,
        });
        app.menu = Some(app.build_menu_screen());
        app
    }

    fn initialized_setup_project_app(repo_root: &std::path::Path) -> App {
        let service = WorktreeService::new(Some(repo_root.to_path_buf()));

        let mut app = App::new(AppMode::Menu, false);
        app.phase = InitPhase::Ready;
        app.screen = Screen::SetupProject;
        app.worktree_service = Some(service);
        app.git_root = Some(repo_root.display().to_string());
        app.setup_project = Some(SetupProjectScreen::new(Some(repo_root)));
        app
    }

    #[test]
    fn new_app_has_no_selected_path() {
        let app = App::new(AppMode::Menu, false);
        assert!(app.selected_path().is_none());
        assert!(!app.is_from_wrapper);
    }

    #[test]
    fn new_app_remembers_from_wrapper_flag() {
        let app = App::new(AppMode::Dashboard, true);
        assert!(app.is_from_wrapper);
        assert!(app.selected_path().is_none());
    }

    #[test]
    fn menu_create_selection_enters_create_screen() {
        with_home(|_| {
            let mut app = initialized_menu_app();
            let tx = app_event_tx();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                app.handle_key(key(KeyCode::Enter), &tx);
                tokio::task::yield_now().await;
            });

            assert_eq!(app.screen, Screen::Create);
            assert!(app.create.is_some());
            assert!(app.menu.is_none());
        });
    }

    #[test]
    fn menu_settings_selection_enters_settings_screen() {
        with_home(|_| {
            let mut app = initialized_menu_app();
            let tx = app_event_tx();

            app.handle_key(key(KeyCode::Down), &tx);
            app.handle_key(key(KeyCode::Down), &tx);
            app.handle_key(key(KeyCode::Enter), &tx);

            assert_eq!(app.screen, Screen::Settings);
            assert!(app.settings.is_some());
        });
    }

    #[test]
    fn settings_delete_branch_toggle_updates_global_config_file_when_local_missing() {
        with_home(|home| {
            // Use a repo dir inside the temp home so has_local_config() is
            // deterministically false (no .wisetree.json there).
            let repo_root = home.path().join("repo_no_local");
            fs::create_dir_all(&repo_root).unwrap();

            let mut config_service = ConfigService::new();
            let global_path = home.path().join(".wisetree").join("settings.json");
            let initial = WorktreeConfig {
                terminal_command: "code $WORKTREE_PATH".into(),
                delete_branch_with_worktree: false,
                ..WorktreeConfig::default()
            };
            config_service.save(&initial, Some(&global_path)).unwrap();

            let mut service = WorktreeService::new(None);
            service.config_service_mut().load_global().unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.screen = Screen::Settings;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            let tx = app_event_tx();
            app.enter_screen(Screen::Settings, &tx);
            for _ in 0..12 {
                app.handle_key(key(KeyCode::Down), &tx);
            }
            app.handle_key(key(KeyCode::Enter), &tx);
            app.handle_key(key(KeyCode::Char('y')), &tx);
            app.handle_key(key(KeyCode::Enter), &tx);

            let saved: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();
            assert!(saved.delete_branch_with_worktree);
            assert_eq!(saved.terminal_command, "code $WORKTREE_PATH");
            assert_eq!(
                app.settings.as_ref().unwrap().config_path(),
                global_path.display().to_string()
            );
            assert!(
                app.settings
                    .as_ref()
                    .unwrap()
                    .config()
                    .delete_branch_with_worktree
            );
            assert!(
                app.worktree_service
                    .as_ref()
                    .unwrap()
                    .config_service()
                    .config()
                    .delete_branch_with_worktree
            );
        });
    }

    #[test]
    fn settings_delete_branch_toggle_updates_local_config_file_when_local_exists() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                terminal_command: "global $WORKTREE_PATH".into(),
                delete_branch_with_worktree: false,
                ..WorktreeConfig::default()
            };
            let local = WorktreeConfig {
                terminal_command: "local $WORKTREE_PATH".into(),
                delete_branch_with_worktree: false,
                ..WorktreeConfig::default()
            };

            let mut writer = ConfigService::new();
            writer.save(&global, Some(&global_path)).unwrap();
            writer.save(&local, Some(&local_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.screen = Screen::Settings;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            let tx = app_event_tx();
            app.enter_screen(Screen::Settings, &tx);

            for _ in 0..11 {
                app.handle_key(key(KeyCode::Down), &tx);
            }
            app.handle_key(key(KeyCode::Enter), &tx);
            app.handle_key(key(KeyCode::Char('y')), &tx);
            app.handle_key(key(KeyCode::Enter), &tx);

            let saved_local: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&local_path).unwrap()).unwrap();
            assert!(saved_local.delete_branch_with_worktree);
            assert_eq!(saved_local.terminal_command, "local $WORKTREE_PATH");

            let saved_global: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();
            assert!(!saved_global.delete_branch_with_worktree);
            assert_eq!(saved_global.terminal_command, "global $WORKTREE_PATH");

            assert_eq!(
                app.settings.as_ref().unwrap().config_path(),
                local_path.display().to_string()
            );
            assert!(
                app.settings
                    .as_ref()
                    .unwrap()
                    .config()
                    .delete_branch_with_worktree
            );
            assert!(
                app.worktree_service
                    .as_ref()
                    .unwrap()
                    .config_service()
                    .config()
                    .delete_branch_with_worktree
            );
            assert_eq!(
                app.worktree_service
                    .as_ref()
                    .unwrap()
                    .config_service()
                    .config_path(),
                Some(local_path.as_path())
            );
        });
    }

    #[test]
    fn settings_reenter_uses_local_delete_branch_value_when_local_exists() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                delete_branch_with_worktree: false,
                ..WorktreeConfig::default()
            };
            let local = WorktreeConfig {
                delete_branch_with_worktree: false,
                ..WorktreeConfig::default()
            };

            let mut writer = ConfigService::new();
            writer.save(&global, Some(&global_path)).unwrap();
            writer.save(&local, Some(&local_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.screen = Screen::Settings;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            let tx = app_event_tx();
            app.enter_screen(Screen::Settings, &tx);

            for _ in 0..11 {
                app.handle_key(key(KeyCode::Down), &tx);
            }
            app.handle_key(key(KeyCode::Enter), &tx);
            app.handle_key(key(KeyCode::Char('y')), &tx);
            app.handle_key(key(KeyCode::Enter), &tx);

            app.back_to_menu();
            app.enter_screen(Screen::Settings, &tx);

            assert_eq!(
                app.settings.as_ref().unwrap().config_path(),
                local_path.display().to_string()
            );
            assert!(
                app.settings
                    .as_ref()
                    .unwrap()
                    .config()
                    .delete_branch_with_worktree
            );
        });
    }

    #[test]
    fn settings_edit_file_path_prefers_global_when_local_config_is_missing() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.git_root = Some(repo_root.display().to_string());

            assert_eq!(
                app.settings_edit_file_path(),
                home.path().join(".wisetree").join("settings.json")
            );
            assert_eq!(
                SETTINGS_PATH_COPIED_MESSAGE,
                "Setting file copied to Clipboard, edit it with your favorite editor!"
            );
        });
    }

    #[test]
    fn settings_edit_file_path_prefers_local_when_local_config_exists() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);
            fs::write(&local_path, "{}\n").unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.git_root = Some(repo_root.display().to_string());

            assert_eq!(app.settings_edit_file_path(), local_path);
        });
    }

    #[test]
    fn settings_copy_global_to_local_creates_local_config_file() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let global = WorktreeConfig {
                terminal_command: "global $WORKTREE_PATH".into(),
                delete_branch_with_worktree: true,
                post_create_cmd: vec!["bun install".into()],
                ..WorktreeConfig::default()
            };

            let mut config_service = ConfigService::new();
            config_service.save(&global, Some(&global_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load_global().unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.screen = Screen::Settings;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            let tx = app_event_tx();
            app.enter_screen(Screen::Settings, &tx);

            for _ in 0..11 {
                app.handle_key(key(KeyCode::Down), &tx);
            }
            app.handle_key(key(KeyCode::Enter), &tx);
            app.handle_key(key(KeyCode::Enter), &tx);

            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);
            let saved: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&local_path).unwrap()).unwrap();

            assert_eq!(saved, global);
            assert_eq!(
                app.worktree_service
                    .as_ref()
                    .unwrap()
                    .config_service()
                    .config_path(),
                Some(local_path.as_path())
            );
        });
    }

    #[test]
    fn settings_copy_local_to_global_overwrites_global_config_file() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                terminal_command: "global".into(),
                delete_branch_with_worktree: false,
                post_create_cmd: vec!["npm install".into()],
                ..WorktreeConfig::default()
            };
            let local = WorktreeConfig {
                terminal_command: "local".into(),
                delete_branch_with_worktree: true,
                post_create_cmd: vec!["bun install".into(), "bun test".into()],
                ..WorktreeConfig::default()
            };

            let mut config_service = ConfigService::new();
            config_service.save(&global, Some(&global_path)).unwrap();
            config_service.save(&local, Some(&local_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.screen = Screen::Settings;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            let tx = app_event_tx();
            app.enter_screen(Screen::Settings, &tx);

            for _ in 0..10 {
                app.handle_key(key(KeyCode::Down), &tx);
            }
            app.handle_key(key(KeyCode::Enter), &tx);
            app.handle_key(key(KeyCode::Down), &tx);
            app.handle_key(key(KeyCode::Enter), &tx);

            let saved: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();

            assert_eq!(saved, local);
            assert_eq!(
                app.settings.as_ref().unwrap().config().terminal_command,
                local.terminal_command
            );
            assert_eq!(
                app.worktree_service
                    .as_ref()
                    .unwrap()
                    .config_service()
                    .config(),
                &local
            );
        });
    }

    #[test]
    fn save_terminal_command_writes_to_local_when_local_exists() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                terminal_command: "global-cmd".into(),
                ..WorktreeConfig::default()
            };
            let local = WorktreeConfig {
                terminal_command: "old-local".into(),
                ..WorktreeConfig::default()
            };
            let mut writer = ConfigService::new();
            writer.save(&global, Some(&global_path)).unwrap();
            writer.save(&local, Some(&local_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            app.save_terminal_command("new-local".into()).unwrap();

            let saved_local: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&local_path).unwrap()).unwrap();
            assert_eq!(saved_local.terminal_command, "new-local");

            let saved_global: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();
            assert_eq!(saved_global.terminal_command, "global-cmd");

            // "Open with Command" reads from current_config() — confirm it sees
            // the just-saved local value.
            assert_eq!(app.current_config().unwrap().terminal_command, "new-local");
        });
    }

    #[test]
    fn save_terminal_command_writes_to_global_when_local_missing() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                terminal_command: "old-global".into(),
                ..WorktreeConfig::default()
            };
            let mut writer = ConfigService::new();
            writer.save(&global, Some(&global_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            app.save_terminal_command("new-global".into()).unwrap();

            assert!(!local_path.exists(), "local config must not be created");

            let saved_global: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();
            assert_eq!(saved_global.terminal_command, "new-global");

            assert_eq!(app.current_config().unwrap().terminal_command, "new-global");
        });
    }

    #[test]
    fn save_path_template_writes_to_local_when_local_exists() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                worktree_path_template: "$BASE_PATH-global".into(),
                ..WorktreeConfig::default()
            };
            let local = WorktreeConfig {
                worktree_path_template: "$BASE_PATH-old".into(),
                ..WorktreeConfig::default()
            };
            let mut writer = ConfigService::new();
            writer.save(&global, Some(&global_path)).unwrap();
            writer.save(&local, Some(&local_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            app.save_path_template("$BASE_PATH-new".into()).unwrap();

            let saved_local: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&local_path).unwrap()).unwrap();
            assert_eq!(saved_local.worktree_path_template, "$BASE_PATH-new");

            let saved_global: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();
            assert_eq!(saved_global.worktree_path_template, "$BASE_PATH-global");

            assert_eq!(
                app.current_config().unwrap().worktree_path_template,
                "$BASE_PATH-new"
            );
        });
    }

    #[test]
    fn save_path_template_writes_to_global_when_local_missing() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                worktree_path_template: "$BASE_PATH-old".into(),
                ..WorktreeConfig::default()
            };
            let mut writer = ConfigService::new();
            writer.save(&global, Some(&global_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            app.save_path_template("$BASE_PATH-new".into()).unwrap();

            assert!(!local_path.exists(), "local config must not be created");

            let saved_global: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();
            assert_eq!(saved_global.worktree_path_template, "$BASE_PATH-new");

            assert_eq!(
                app.current_config().unwrap().worktree_path_template,
                "$BASE_PATH-new"
            );
        });
    }

    #[test]
    fn save_link_strategy_writes_to_local_when_local_exists() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                worktree_link_strategy: LinkStrategy::CreateEmpty,
                ..WorktreeConfig::default()
            };
            let local = WorktreeConfig {
                worktree_link_strategy: LinkStrategy::SeedIfPresent,
                ..WorktreeConfig::default()
            };
            let mut writer = ConfigService::new();
            writer.save(&global, Some(&global_path)).unwrap();
            writer.save(&local, Some(&local_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            app.save_link_strategy(LinkStrategy::SeedFromSource)
                .unwrap();

            let saved_local: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&local_path).unwrap()).unwrap();
            assert_eq!(
                saved_local.worktree_link_strategy,
                LinkStrategy::SeedFromSource
            );

            let saved_global: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();
            assert_eq!(
                saved_global.worktree_link_strategy,
                LinkStrategy::CreateEmpty
            );

            assert_eq!(
                app.current_config().unwrap().worktree_link_strategy,
                LinkStrategy::SeedFromSource
            );
        });
    }

    #[test]
    fn save_link_strategy_writes_to_global_when_local_missing() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                worktree_link_strategy: LinkStrategy::CreateEmpty,
                ..WorktreeConfig::default()
            };
            let mut writer = ConfigService::new();
            writer.save(&global, Some(&global_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            app.save_link_strategy(LinkStrategy::SeedIfPresent).unwrap();

            assert!(!local_path.exists(), "local config must not be created");

            let saved_global: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();
            assert_eq!(
                saved_global.worktree_link_strategy,
                LinkStrategy::SeedIfPresent
            );
            assert_eq!(
                app.current_config().unwrap().worktree_link_strategy,
                LinkStrategy::SeedIfPresent
            );
        });
    }

    #[test]
    fn save_link_cache_dir_writes_to_local_when_local_exists() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                worktree_link_cache_dir: Some("/global/cache".into()),
                ..WorktreeConfig::default()
            };
            let local = WorktreeConfig {
                worktree_link_cache_dir: Some("/local/old-cache".into()),
                ..WorktreeConfig::default()
            };
            let mut writer = ConfigService::new();
            writer.save(&global, Some(&global_path)).unwrap();
            writer.save(&local, Some(&local_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            app.save_link_cache_dir("/local/new-cache".into()).unwrap();

            let saved_local: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&local_path).unwrap()).unwrap();
            assert_eq!(
                saved_local.worktree_link_cache_dir,
                Some("/local/new-cache".into())
            );

            let saved_global: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();
            assert_eq!(
                saved_global.worktree_link_cache_dir,
                Some("/global/cache".into())
            );

            assert_eq!(
                app.current_config().unwrap().worktree_link_cache_dir,
                Some("/local/new-cache".into())
            );
        });
    }

    #[test]
    fn save_link_cache_dir_writes_to_global_when_local_missing() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                worktree_link_cache_dir: Some("/global/old-cache".into()),
                ..WorktreeConfig::default()
            };
            let mut writer = ConfigService::new();
            writer.save(&global, Some(&global_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            app.save_link_cache_dir(String::new()).unwrap();

            assert!(!local_path.exists(), "local config must not be created");

            let saved_global: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();
            assert_eq!(saved_global.worktree_link_cache_dir, None);
            assert_eq!(app.current_config().unwrap().worktree_link_cache_dir, None);
        });
    }

    #[test]
    fn save_dashboard_writes_to_local_when_local_exists() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                dashboard: DashboardConfig {
                    refresh_interval_ms: 5000,
                    show_pull_requests: false,
                    wise_merge: false,
                    columns: vec!["branch".into(), "status".into()],
                    ai: Default::default(),
                    ai_status: Default::default(),
                    legacy_notifications: None,
                },
                ..WorktreeConfig::default()
            };
            let local = WorktreeConfig {
                dashboard: DashboardConfig {
                    refresh_interval_ms: 6000,
                    show_pull_requests: false,
                    wise_merge: false,
                    columns: vec!["branch".into()],
                    ai: Default::default(),
                    ai_status: Default::default(),
                    legacy_notifications: None,
                },
                ..WorktreeConfig::default()
            };
            let mut writer = ConfigService::new();
            writer.save(&global, Some(&global_path)).unwrap();
            writer.save(&local, Some(&local_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            let new_dashboard = DashboardConfig {
                refresh_interval_ms: 7000,
                show_pull_requests: true,
                wise_merge: true,
                columns: vec![
                    "branch".into(),
                    "status".into(),
                    "ai_status".into(),
                    "pull_request".into(),
                ],
                ai: Default::default(),
                ai_status: Default::default(),
                legacy_notifications: None,
            };
            app.save_dashboard(new_dashboard.clone()).unwrap();

            let saved_local: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&local_path).unwrap()).unwrap();
            assert_eq!(saved_local.dashboard, new_dashboard);

            let saved_global: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();
            assert_eq!(saved_global.dashboard, global.dashboard);

            assert_eq!(app.current_config().unwrap().dashboard, new_dashboard);
        });
    }

    #[test]
    fn save_dashboard_writes_to_global_when_local_missing() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                dashboard: DashboardConfig {
                    refresh_interval_ms: 5000,
                    show_pull_requests: false,
                    wise_merge: false,
                    columns: vec!["branch".into()],
                    ai: Default::default(),
                    ai_status: Default::default(),
                    legacy_notifications: None,
                },
                ..WorktreeConfig::default()
            };
            let mut writer = ConfigService::new();
            writer.save(&global, Some(&global_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            let new_dashboard = DashboardConfig {
                refresh_interval_ms: 8000,
                show_pull_requests: true,
                wise_merge: false,
                columns: vec!["branch".into(), "status".into(), "ai_status".into()],
                ai: Default::default(),
                ai_status: Default::default(),
                legacy_notifications: None,
            };
            app.save_dashboard(new_dashboard.clone()).unwrap();

            assert!(!local_path.exists(), "local config must not be created");

            let saved_global: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();
            assert_eq!(saved_global.dashboard, new_dashboard);

            assert_eq!(app.current_config().unwrap().dashboard, new_dashboard);
        });
    }

    #[test]
    fn save_dashboard_wise_merge_change_writes_to_local_when_local_missing() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let global_path = home.path().join(".wisetree").join("settings.json");
            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);

            let global = WorktreeConfig {
                dashboard: DashboardConfig {
                    refresh_interval_ms: 5000,
                    show_pull_requests: true,
                    wise_merge: false,
                    columns: vec!["branch".into(), "status".into()],
                    ai: Default::default(),
                    ai_status: Default::default(),
                    legacy_notifications: None,
                },
                terminal_command: "global-terminal".into(),
                ..WorktreeConfig::default()
            };
            let mut writer = ConfigService::new();
            writer.save(&global, Some(&global_path)).unwrap();

            let mut service = WorktreeService::new(Some(repo_root.clone()));
            service.config_service_mut().load(Some(&repo_root)).unwrap();

            let mut app = App::new(AppMode::Settings, false);
            app.phase = InitPhase::Ready;
            app.worktree_service = Some(service);
            app.git_root = Some(repo_root.display().to_string());

            let mut new_dashboard = app.current_config().unwrap().dashboard.clone();
            new_dashboard.wise_merge = true;
            app.save_dashboard(new_dashboard.clone()).unwrap();

            let saved_local: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&local_path).unwrap()).unwrap();
            assert_eq!(saved_local.dashboard, new_dashboard);
            assert_eq!(saved_local.terminal_command, "global-terminal");

            let saved_global: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).unwrap();
            assert_eq!(saved_global.dashboard, global.dashboard);

            assert_eq!(app.current_config().unwrap().dashboard, new_dashboard);
        });
    }

    #[test]
    fn ctrl_c_quits_without_emitting_path() {
        let mut app = App::new(AppMode::Dashboard, true);
        app.phase = InitPhase::Ready;
        let tx = app_event_tx();
        let ctrl_c = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_key(ctrl_c, &tx);
        assert!(app.quit_requested);
        assert!(app.selected_path().is_none());
    }

    #[test]
    fn create_finished_returns_to_menu_with_success_toast() {
        let mut app = initialized_menu_app();
        app.screen = Screen::Create;
        app.menu = None;
        app.create = Some(CreateScreen::new());
        if let Some(create) = app.create.as_mut() {
            create.set_branches(Vec::new());
            create.navigate_after_create = false;
        }

        app.handle_app_event(
            AppEvent::CreateFinished(Ok(ServiceCreateOutcome {
                worktree_path: PathBuf::from("/tmp/repo/feat-x"),
                ..ServiceCreateOutcome::default()
            })),
            &app_event_tx(),
        );

        assert_eq!(app.screen, Screen::Create);
        assert!(app.toast.current().is_none());

        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();

        let dumped = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(dumped.contains("Worktree created successfully"));
        assert!(dumped.contains("Worktree path: /tmp/repo/feat-x"));

        app.handle_key(key(KeyCode::Enter), &app_event_tx());

        assert_eq!(app.screen, Screen::Menu);
        assert!(app.create.is_none());

        let toast = app.toast.current().expect("toast should be shown");
        assert_eq!(toast.variant, ToastVariant::Success);
        assert!(dumped.contains(CREATE_SUCCESS));
    }

    #[test]
    fn create_finished_in_wrapper_mode_selects_path_and_quits() {
        let service = WorktreeService::new(None);
        let mut app = App::new(AppMode::Create, true);
        app.phase = InitPhase::Ready;
        app.screen = Screen::Create;
        app.worktree_service = Some(service);
        app.git_root = Some("/tmp/repo".into());
        app.create = Some(CreateScreen::new());
        if let Some(create) = app.create.as_mut() {
            create.set_branches(Vec::new());
        }

        app.handle_app_event(
            AppEvent::CreateFinished(Ok(ServiceCreateOutcome {
                worktree_path: PathBuf::from("/tmp/repo/feat-x"),
                ..ServiceCreateOutcome::default()
            })),
            &app_event_tx(),
        );

        app.handle_key(key(KeyCode::Enter), &app_event_tx());

        assert!(app.quit_requested);
        assert_eq!(app.selected_path(), Some("/tmp/repo/feat-x"));
    }

    #[test]
    fn paste_fills_directory_input_and_drops_control_chars() {
        let mut app = initialized_menu_app();
        app.screen = Screen::Create;
        app.menu = None;
        app.create = Some(CreateScreen::new());
        if let Some(create) = app.create.as_mut() {
            create.set_branches(Vec::new());
        }

        // Simulate a bracketed paste carrying a trailing newline (as most
        // clipboard copies do). The newline must not submit the prompt.
        app.handle_paste("pasted-dir-name\n".to_string(), &app_event_tx());

        // Still on the directory-input step — the newline was dropped, not
        // treated as Enter.
        assert_eq!(app.screen, Screen::Create);

        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let dumped = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            dumped.contains("pasted-dir-name"),
            "pasted text should appear in the input: {dumped}"
        );
    }

    #[test]
    fn explain_opening_terminal_activity_uses_full_height_panel() {
        let mut app = initialized_menu_app();
        app.screen = Screen::ExplainPullRequest;
        app.explain_pr = Some(ExplainPullRequestScreen::new(
            ExplainPullRequestRequest {
                branch: "feature/explain".into(),
                worktree_path: "/tmp/repo/feature/explain".into(),
                base_ref: Some("upstream/main".into()),
                pr_base_ref: None,
                number: None,
                title: None,
                url: None,
                existing_labels: Vec::new(),
            },
            crate::config::schema::AiModelConfig::default(),
        ));
        let screen = app.explain_pr.as_mut().unwrap();
        screen.start_opening();
        screen.append_terminal_line("running tests".into(), crate::files::ActivityKind::Stdout);

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let dump = rows.join("\n");

        assert!(dump.contains("Opening pull request"), "{dump}");
        assert!(dump.contains("Terminal Activity"), "{dump}");
        assert!(
            !rows.last().unwrap().trim().is_empty(),
            "Fill Opening must occupy the full bottom panel so streaming output stays framed:\n{dump}"
        );
    }

    #[test]
    fn delete_finished_with_branch_warning_shows_toast() {
        let mut app = initialized_menu_app();
        app.screen = Screen::Delete;
        app.delete = Some(DeleteScreen::new(true));

        app.handle_app_event(
            AppEvent::DeleteFinished(Ok(ServiceDeleteOutcome {
                worktree_deleted: true,
                branch_deleted: false,
                branch_name: Some("ignore-local".into()),
                branch_delete_error: Some(
                    "Branch 'ignore-local' was kept.\nerror: the branch 'ignore-local' is not fully merged"
                        .into(),
                ),
            })),
            &app_event_tx(),
        );

        let toast = app.toast.current().expect("toast should be shown");
        assert_eq!(toast.variant, ToastVariant::Warning);
        assert!(toast.message.contains("ignore-local"));
        assert!(toast.message.contains("not fully merged"));
        assert_eq!(
            app.delete.as_ref().unwrap().step(),
            screens::delete::DeleteStep::Success
        );
    }

    #[test]
    fn create_summary_rows_flattens_reports_and_command_runs() {
        use crate::files::{CommandRun, CopyReport, LinkReport, LinkedEntry};

        let outcome = ServiceCreateOutcome {
            worktree_path: PathBuf::from("/tmp/repo/feat-x"),
            copy_report: Some(CopyReport {
                copied: vec![".env".into(), ".envrc".into()],
                skipped: vec!["node_modules".into()],
                errors: Vec::new(),
            }),
            link_report: Some(LinkReport {
                linked: vec![LinkedEntry {
                    pattern: ".cache".into(),
                    cache_path: PathBuf::from("/cache/.cache"),
                    link_path: PathBuf::from("/tmp/repo/feat-x/.cache"),
                    seeded: false,
                }],
                skipped: Vec::new(),
                errors: vec!["link broke".into()],
            }),
            command_runs: vec![
                CommandRun {
                    command: "bun install".into(),
                    success: true,
                    output: String::new(),
                    error: None,
                },
                CommandRun {
                    command: "install_skills".into(),
                    success: false,
                    output: String::new(),
                    error: Some("not found".into()),
                },
            ],
            terminal_launch: None,
        };

        let rows = create_summary_rows(&outcome);

        assert_eq!(rows.len(), 5);
        // Copy patterns succeeded.
        assert_eq!(rows[0].command, "Copy patterns (2 copied)");
        assert!(rows[0].success);
        assert!(rows[0].failure.is_none());
        // Ignore patterns row only appears when some files were skipped.
        assert_eq!(rows[1].command, "Ignore patterns (1 skipped)");
        assert!(rows[1].success);
        // Link patterns failed with the explicit error.
        assert_eq!(rows[2].command, "Link patterns (1 linked)");
        assert!(!rows[2].success);
        assert_eq!(rows[2].failure.as_deref(), Some("link broke"));
        // Post-create commands appear in order.
        assert_eq!(rows[3].command, "bun install");
        assert!(rows[3].success);
        assert_eq!(rows[4].command, "install_skills");
        assert!(!rows[4].success);
        assert_eq!(rows[4].failure.as_deref(), Some("not found"));
    }

    #[test]
    fn create_finished_renders_summary_table_with_status_icons() {
        use crate::files::CommandRun;

        let mut app = initialized_menu_app();
        app.screen = Screen::Create;
        app.menu = None;
        app.create = Some(CreateScreen::new());
        if let Some(create) = app.create.as_mut() {
            create.set_branches(Vec::new());
            create.navigate_after_create = false;
        }

        app.handle_app_event(
            AppEvent::CreateFinished(Ok(ServiceCreateOutcome {
                worktree_path: PathBuf::from("/tmp/repo/feat-x"),
                command_runs: vec![
                    CommandRun {
                        command: "bun install".into(),
                        success: true,
                        output: String::new(),
                        error: None,
                    },
                    CommandRun {
                        command: "install_skills".into(),
                        success: false,
                        output: String::new(),
                        error: Some("not found".into()),
                    },
                ],
                ..ServiceCreateOutcome::default()
            })),
            &app_event_tx(),
        );

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();

        let dumped = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(dumped.contains("Command"));
        assert!(dumped.contains("Status"));
        assert!(dumped.contains("Failure"));
        assert!(dumped.contains("bun install"));
        assert!(dumped.contains("install_skills"));
        assert!(dumped.contains("not found"));
        assert!(dumped.contains("✅"));
        assert!(dumped.contains("❌"));
        assert!(dumped.contains("None"));
    }

    #[test]
    fn bulk_delete_esc_from_selection_returns_to_dashboard() {
        with_home(|_| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let mut app = initialized_menu_app();
                app.screen = Screen::Delete;
                app.menu = None;

                let mut delete = DeleteScreen::new(false);
                delete.set_worktrees(vec![
                    GitWorktree {
                        path: "/tmp/repo".into(),
                        branch: "main".into(),
                        commit: "deadbeef".into(),
                        is_main: true,
                        is_clean: true,
                        branch_status: None,
                    },
                    GitWorktree {
                        path: "/tmp/repo-feat".into(),
                        branch: "feat".into(),
                        commit: "deadbeef".into(),
                        is_main: false,
                        is_clean: true,
                        branch_status: None,
                    },
                ]);
                delete.jump_to_bulk_confirm(vec!["/tmp/repo-feat".into()]);
                app.delete = Some(delete);

                app.handle_delete_key(key(KeyCode::Esc), &app_event_tx());
                tokio::task::yield_now().await;

                assert_eq!(app.screen, Screen::Dashboard);
                assert!(app.dashboard.is_some());
            });
        });
    }

    #[test]
    fn wise_preset_discovery_completion_moves_screen_to_confirm_and_shows_toast() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            write(&repo_root, "api/Gemfile", "");
            write(&repo_root, "api/config/application.rb", "");
            write(&repo_root, "api/config/master.key", "secret");
            write(
                &repo_root,
                "web/package.json",
                "{\"dependencies\": {\"react\": \"18\"}}",
            );
            write(&repo_root, "web/.env.local", "VITE_X=1");

            let mut app = initialized_setup_project_app(&repo_root);
            let discovery =
                crate::services::presets::discover_wise(&repo_root).expect("wise preset");

            app.apply_wise_preset_discovery(Ok(discovery));

            assert_eq!(
                app.setup_project.as_ref().unwrap().step(),
                SetupProjectStep::Confirm
            );
            let toast = app.toast.current().expect("toast should be shown");
            assert_eq!(toast.variant, ToastVariant::Success);
            assert!(toast.message.contains("Ruby on Rails"));
            assert!(toast.message.contains("React (CRA / Vite)"));
        });
    }

    #[test]
    fn wise_preset_generic_fallback_shows_warning_toast() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            fs::create_dir_all(&repo_root).unwrap();

            let mut app = initialized_setup_project_app(&repo_root);
            let discovery =
                crate::services::presets::discover_wise(&repo_root).expect("wise preset");

            app.apply_wise_preset_discovery(Ok(discovery));

            assert_eq!(
                app.setup_project.as_ref().unwrap().step(),
                SetupProjectStep::Confirm
            );
            let toast = app.toast.current().expect("toast should be shown");
            assert_eq!(toast.variant, ToastVariant::Warning);
            assert!(toast.message.contains("Generic"));
        });
    }

    #[test]
    fn wise_preset_apply_writes_local_config_and_preserves_other_values() {
        with_home(|home| {
            let repo_root = home.path().join("repo");
            write(&repo_root, "api/Gemfile", "");
            write(&repo_root, "api/config/application.rb", "");
            write(&repo_root, "api/config/master.key", "secret");
            write(
                &repo_root,
                "web/package.json",
                "{\"dependencies\": {\"react\": \"18\"}}",
            );
            write(&repo_root, "web/.env.local", "VITE_X=1");

            let mut app = initialized_setup_project_app(&repo_root);
            app.worktree_service
                .as_mut()
                .unwrap()
                .config_service_mut()
                .update(|config| {
                    config.terminal_command = "code $WORKTREE_PATH".into();
                    config.delete_branch_with_worktree = true;
                    config.dashboard.show_pull_requests = true;
                });

            let discovery =
                crate::services::presets::discover_wise(&repo_root).expect("wise preset");
            app.apply_wise_preset_discovery(Ok(discovery));
            app.handle_setup_project_key(key(KeyCode::Enter), &app_event_tx());

            let local_path = repo_root.join(LOCAL_CONFIG_FILE_NAME);
            let saved: WorktreeConfig =
                serde_json::from_str(&fs::read_to_string(&local_path).unwrap()).unwrap();

            assert_eq!(app.screen, Screen::Menu);
            assert_eq!(saved.terminal_command, "code $WORKTREE_PATH");
            assert!(saved.delete_branch_with_worktree);
            assert!(saved.dashboard.show_pull_requests);
            assert!(saved
                .worktree_copy_patterns
                .iter()
                .any(|pattern| pattern == "api/config/master.key"));
            assert!(saved
                .worktree_copy_patterns
                .iter()
                .any(|pattern| pattern == "web/.env.local"));
            assert!(saved
                .worktree_copy_ignores
                .iter()
                .any(|pattern| pattern == "api/**/vendor/bundle/**"));
            assert!(saved
                .worktree_copy_ignores
                .iter()
                .any(|pattern| pattern == "web/**/node_modules/**"));
            assert!(saved
                .worktree_link_patterns
                .iter()
                .any(|pattern| pattern == "api/vendor/bundle"));
            assert!(saved
                .worktree_link_patterns
                .iter()
                .any(|pattern| pattern == "web/node_modules"));
            assert_eq!(saved.worktree_link_strategy, LinkStrategy::SeedFromSource);
            assert!(saved.post_create_cmd.iter().any(|command| {
                command == "(cd 'api' && bundle install --jobs 5 --verbose --retry 4)"
            }));
            assert!(saved
                .post_create_cmd
                .iter()
                .any(|command| command == "(cd 'web' && npm install)"));

            let toast = app.toast.current().expect("toast should be shown");
            assert_eq!(toast.variant, ToastVariant::Success);
            assert_eq!(toast.message, "Applied Wise Preset to .wisetree.json");
        });
    }

    #[test]
    fn mouse_wheel_scroll_routes_into_setup_project_confirm_blocks() {
        let repo_root = tempfile::tempdir().unwrap().keep();
        let mut app = initialized_setup_project_app(&repo_root);
        app.setup_project.as_mut().unwrap().complete_wise_discovery(
            crate::services::presets::WisePresetDiscovery {
                matched_ids: vec![crate::services::presets::PresetId::Generic],
                copy_patterns: vec![
                    "copy-1".into(),
                    "copy-2".into(),
                    "copy-3".into(),
                    "copy-4".into(),
                    "copy-5".into(),
                    "copy-6".into(),
                ],
                copy_ignores: vec!["ignore-1".into()],
                link_patterns: vec!["links-1".into()],
                post_create_cmd: vec!["cmd-1".into()],
            },
        );

        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let completed = terminal.draw(|frame| app.draw(frame)).unwrap();
        app.last_rendered_buffer = Some(completed.buffer.clone());
        let initial = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(initial.contains("copy-1"));

        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 5, 8), &app_event_tx());
        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 5, 8), &app_event_tx());

        let completed = terminal.draw(|frame| app.draw(frame)).unwrap();
        app.last_rendered_buffer = Some(completed.buffer.clone());
        let scrolled = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!scrolled.contains("copy-1"));
        assert!(scrolled.contains("copy-3"));
        assert!(scrolled.contains("Yes"));
        assert!(scrolled.contains("No"));
    }

    #[test]
    fn draw_renders_active_toast_overlay() {
        let mut app = initialized_menu_app();
        app.show_toast(ToastVariant::Info, "Copied to clipboard");

        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();

        let dumped = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(dumped.contains("Choose wisely"));
        assert!(dumped.contains("Copied to clipboard"));
    }

    #[test]
    fn update_all_branches_finishes_when_queue_drains() {
        let mut app = App::new(AppMode::Dashboard, false);
        // A `None` git root makes the return-to-dashboard step skip spawning a
        // watch, keeping the test free of a Tokio runtime.
        app.git_root = None;
        // Final worktree of a two-item Branches run: one already updated, the
        // queue now empty.
        app.update_all = Some(UpdateAllRun {
            kind: UpdateAllKind::Branches,
            branch_queue: Vec::new(),
            pr_queue: Vec::new(),
            total: 2,
            updated: 1,
            resolved: 0,
            skipped: 0,
            failed: Vec::new(),
        });
        app.handle_app_event(
            AppEvent::UpdateBranchFinished(Ok(UpdateBranchOutcome::AlreadyUpToDate {
                base_ref: "origin/main".to_string(),
            })),
            &app_event_tx(),
        );
        assert!(
            app.update_all.is_none(),
            "batch should end when queue drains"
        );
        assert_eq!(app.screen, Screen::Dashboard);
        let toast = app.toast.current().expect("summary toast");
        assert_eq!(toast.variant, ToastVariant::Success);
    }

    #[test]
    fn update_all_branches_failure_yields_warning_summary() {
        let mut app = App::new(AppMode::Dashboard, false);
        app.git_root = None;
        app.update_all = Some(UpdateAllRun {
            kind: UpdateAllKind::Branches,
            branch_queue: Vec::new(),
            pr_queue: Vec::new(),
            total: 1,
            updated: 0,
            resolved: 0,
            skipped: 0,
            failed: Vec::new(),
        });
        app.handle_app_event(
            AppEvent::UpdateBranchFinished(Ok(UpdateBranchOutcome::FetchFailed(
                "network down".to_string(),
            ))),
            &app_event_tx(),
        );
        assert!(app.update_all.is_none());
        // A recorded failure downgrades the summary to a warning.
        let toast = app.toast.current().expect("failure toast");
        assert_eq!(toast.variant, ToastVariant::Warning);
    }
}

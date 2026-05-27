//! `App` — central TUI state machine.
//!
//! Owns screen routing, per-screen async work, and the wrapper-mode selected
//! path handoff used by shell integration.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{env, ffi::OsString};

#[cfg(unix)]
use libc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;
use tokio::sync::mpsc;

use crate::cli::AppMode;
use crate::config::schema::{DashboardConfig, LinkStrategy, WorktreeConfig};
use crate::config::service::ConfigService;
use crate::constants::{global_config_file, LOCAL_CONFIG_FILE_NAME};
use crate::errors::user_friendly_message;
use crate::files::service::{open_terminal, open_url};
use crate::git::exec::get_git_root;
use crate::git::service::GitService;
use crate::git::types::{GitBranch, GitWorktree, WorktreeCreateOptions};
use crate::messages::{colors, CREATE_SUCCESS, DELETE_SUCCESS};
use crate::services::presets::WisePresetDiscovery;
use crate::services::{
    check_for_updates, default_dashboard_warning, detect_shell_integration,
    fetch_free_opencode_models, fetch_opencode_models, install_shell_integration,
    resolve_dashboard_columns, AppStateService, DashboardService, DashboardUpdate, DashboardWatch,
    OpencodeModel, Shell, ShellIntegrationStatus, UpdateBranchOutcome, UpdateCheckResult,
    UpdatePhase, UpdateProgress,
};
use crate::tui::event::{Event, EventLoop};
use crate::tui::router::Screen;
use crate::tui::screens;
use crate::tui::screens::ai_model_picker::{AiModelPickerAction, AiModelPickerScreen};
use crate::tui::screens::cache::{CacheAction as CacheScreenAction, CacheScreen};
use crate::tui::screens::create::{CreateAction, CreateScreen, SummaryRow};
use crate::tui::screens::dashboard::{
    BulkDeleteStatus, ClosePullRequestRequest, DashboardAction, DashboardScreen,
    MergePullRequestRequest, UpdatePullRequestRequest,
};
use crate::tui::screens::delete::{
    DeleteAction, DeleteOutcome as ScreenDeleteOutcome, DeleteScreen, DeleteStep,
};
use crate::tui::screens::menu::{MenuChoice, MenuOutcome, MenuScreen};
use crate::tui::screens::merge_pr::{MergeAction, MergePullRequestScreen, MergeStep};
use crate::tui::screens::settings::{CopyDirection, SettingsAction, SettingsScreen, SettingsStep};
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
use crate::tui::widgets::{render_toast, ToastState, ToastVariant, WelcomeHeader};
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
    SettingsUpdateChecked(UpdateCheckResult),
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
    /// Result of the background fetch that powers the AI provider/model
    /// picker. The picker stays in its loading state until this lands.
    AiModelsFetched(Result<Vec<OpencodeModel>, String>),
    /// Result of the background `opencode models opencode` shell-out that
    /// powers the Dashboard footer's free-model quick-pick.
    FreeOpencodeModelsFetched(Result<Vec<String>, String>),
    ShellIntegrationDetected(ShellIntegrationStatus),
}

struct MergePrDetailsPayload {
    title: String,
    body: String,
}

struct MergePrFailure {
    number: u64,
    message: String,
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
    cache: Option<CacheScreen>,
    create: Option<CreateScreen>,
    delete: Option<DeleteScreen>,
    settings: Option<SettingsScreen>,
    setup: Option<SetupScreen>,
    setup_project: Option<SetupProjectScreen>,
    merge_pr: Option<MergePullRequestScreen>,
    update_pr: Option<UpdatePullRequestScreen>,
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
            cache: None,
            create: None,
            delete: None,
            settings: None,
            setup: None,
            setup_project: None,
            merge_pr: None,
            update_pr: None,
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
            let _ = terminal::restore_wrapper_tty();
            let _ = terminal.show_cursor();
            result?;
        } else {
            let mut terminal = terminal::enter()?;
            let result = self.event_loop(&mut terminal).await;
            let _ = terminal.clear();
            let _ = terminal::restore();
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

            let completed = terminal.draw(|frame| self.draw(frame))?;
            self.last_rendered_buffer = Some(completed.buffer.clone());

            match events.next_event()? {
                Event::Key(key) => self.handle_key(key, &tx),
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
                screens::loading::draw(frame, area, self.tick, self.screen.as_str());
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
                if let Some(modal) = overlay_modal {
                    let panel = self.render_framed_panel_fill(frame, area);
                    if let Some(dashboard) = self.dashboard.as_mut() {
                        dashboard.tick = self.tick;
                        dashboard.render(frame, panel);
                    }
                    modal.render(frame, panel);
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
                    Some(SettingsStep::Menu) | Some(SettingsStep::DeleteBranch) | None => {
                        self.render_framed_panel_fill(frame, area)
                    }
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
                // Once the AI is actively streaming the conflict resolution
                // we want the entire bottom region of the screen so the
                // AI Activity panel — the longest-running, scroll-heavy view
                // in the app — has room to breathe. The Confirm and pre-AI
                // phases (Fetching, Merging) stay in the compact framed
                // panel so they don't look lost in a huge empty area.
                let ai_active = self
                    .update_pr
                    .as_ref()
                    .is_some_and(|s| s.is_updating() && s.ai_active());
                let in_confirm = self
                    .update_pr
                    .as_ref()
                    .is_some_and(|s| matches!(s.step(), UpdateStep::Confirm));
                let panel = if ai_active || in_confirm {
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
        WelcomeHeader::new(self.screen, cwd).render(frame, chunks[0]);

        self.render_panel_block(frame, chunks[1])
    }

    fn render_framed_panel_fill(&self, frame: &mut Frame, area: Rect) -> Rect {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(4), Constraint::Min(0)])
            .split(area);

        let cwd = self.git_root.as_deref().unwrap_or("");
        WelcomeHeader::new(self.screen, cwd).render(frame, chunks[0]);

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

    fn handle_mouse(&mut self, mouse: MouseEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
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
                if matches!(self.screen, Screen::Dashboard) {
                    let action = self
                        .dashboard
                        .as_mut()
                        .map(|dashboard| dashboard.handle_mouse_click(raw_position))
                        .unwrap_or(DashboardAction::Continue);
                    self.apply_dashboard_action(action, tx);
                }
            }
            MouseEventKind::ScrollUp => {
                // Web-page semantics: wheel scrolls the screen's active
                // scrollable region. On Update Pull Request that is either
                // the live AI Activity panel (during conflict resolution) or
                // the review diff panel (after the AI creates a merge commit).
                if matches!(self.screen, Screen::UpdatePullRequest) {
                    if let Some(screen) = self.update_pr.as_mut() {
                        screen.handle_mouse_scroll_up(WHEEL_LINES_PER_TICK);
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                if matches!(self.screen, Screen::UpdatePullRequest) {
                    if let Some(screen) = self.update_pr.as_mut() {
                        screen.handle_mouse_scroll_down(WHEEL_LINES_PER_TICK);
                    }
                }
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
            Screen::UpdateBranch => {
                if let Some(screen) = self.update_branch.as_mut() {
                    screen.handle_key(key);
                }
            }
            Screen::AiModelPicker => self.handle_ai_model_picker_key(key, tx),
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
            AiModelPickerAction::Selected(model) => {
                // Stamp the chosen pair into the still-live Dashboard editor
                // and drop back onto it — the user persists the change by
                // pressing the editor's Save button (same pattern as every
                // other dashboard field). Auto-saving here would route the
                // user past the editor to the Settings menu, which they
                // don't expect.
                if let Some(settings) = self.settings.as_mut() {
                    settings.apply_use_ai_selection(model);
                }
                self.close_ai_model_picker();
                let _ = tx;
            }
        }
    }

    /// Push the picker on top of the still-alive Settings screen, kick off the
    /// background catalogue fetch, and flip the route. The picker reads
    /// `current_use_ai` so reopening the picker lands on the user's prior
    /// choice.
    fn open_ai_model_picker(
        &mut self,
        current_use_ai: String,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        self.ai_model_picker = Some(AiModelPickerScreen::new(current_use_ai));
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
                self.merge_pr = None;
                self.enter_screen(Screen::Dashboard, tx);
            }
            MergeAction::Confirmed {
                number,
                title,
                body,
            } => {
                if let Some(screen) = self.merge_pr.as_mut() {
                    screen.start_merging();
                }
                kick_off_merge_pull_request(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    number,
                    title,
                    body,
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
                self.update_pr = None;
                self.enter_screen(Screen::Dashboard, tx);
            }
            UpdateAction::Confirmed => {
                let Some(screen) = self.update_pr.as_mut() else {
                    return;
                };
                let request = screen.request().clone();
                screen.start_updating();
                kick_off_update_pull_request(
                    self.git_root.clone(),
                    self.current_dashboard_config(),
                    request,
                    tx.clone(),
                );
            }
            UpdateAction::AiComplete => {
                let dashboard_config = self.current_dashboard_config();
                let git_root = self.git_root.clone();
                let Some(screen) = self.update_pr.as_mut() else {
                    return;
                };
                let request = screen.request().clone();
                let use_ai = dashboard_config.use_ai.clone();
                let base_ref = request
                    .base_ref
                    .clone()
                    .unwrap_or_else(|| "upstream/main".to_string());
                screen.set_phase_message("Committing and pushing AI resolution...");
                kick_off_commit_and_push(
                    git_root,
                    dashboard_config,
                    request,
                    use_ai,
                    base_ref,
                    tx.clone(),
                );
            }
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
        }
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
                    MenuChoice::SetupProject => self.enter_screen(Screen::SetupProject, tx),
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
            DashboardAction::OpenTerminal(path) => {
                if let Some(config) = self.current_config() {
                    let launch = open_terminal(&config.terminal_command, &path);
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
            DashboardAction::UpdateBranch(path) => {
                self.start_update_branch_flow(path, tx);
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
    /// merge. The flow ends in `apply_update_branch_finished`, which
    /// returns to the dashboard and toasts the outcome.
    fn start_update_branch_flow(
        &mut self,
        worktree_path: String,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        self.update_branch = Some(UpdateBranchScreen::new(worktree_path.clone()));
        self.screen = Screen::UpdateBranch;
        kick_off_update_branch(self.current_dashboard_config(), worktree_path, tx.clone());
    }

    fn start_merge_pr_flow(
        &mut self,
        request: MergePullRequestRequest,
        tx: &mpsc::UnboundedSender<AppEvent>,
    ) {
        let number = request.number;
        self.merge_pr = Some(MergePullRequestScreen::new(request));
        self.screen = Screen::MergePullRequest;
        kick_off_fetch_pr_details(
            self.git_root.clone(),
            self.current_dashboard_config(),
            number,
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
        // Mount the screen with `base_ref = None` first so the confirm
        // panel renders immediately; the resolver runs in the background
        // and populates the field before the user can answer.
        self.update_pr = Some(UpdatePullRequestScreen::new(request));
        self.screen = Screen::UpdatePullRequest;
        kick_off_resolve_base_ref(worktree_path, number, tx.clone());
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
                self.bulk_delete_queue.clear();
                self.leave_delete_screen(tx);
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
        if (from_dashboard_single || from_dashboard_bulk) && self.git_root.is_some() {
            self.enter_screen(Screen::Dashboard, tx);
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
                if let Err(err) = self.save_dashboard(dashboard) {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.set_error(format!("Failed to save dashboard settings: {err}"));
                    }
                }
            }
            SettingsAction::OpenAiModelPicker(current_use_ai) => {
                self.open_ai_model_picker(current_use_ai, tx);
            }
            SettingsAction::FetchFreeModels => {
                kick_off_fetch_free_opencode_models(tx.clone());
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
        // Drop the loading splash and route back to the dashboard before
        // toasting — the user must land on the screen where the toast
        // appears, otherwise the result would flash on the splash for
        // one frame and vanish.
        self.update_branch = None;
        if matches!(self.screen, Screen::UpdateBranch) {
            self.enter_screen(Screen::Dashboard, tx);
        }
        self.show_update_branch_toast(result);
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
                screen.spawn_opencode_pty(
                    opencode_binary.clone(),
                    opencode_args.clone(),
                    cwd.clone(),
                    Vec::new(),
                );
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
                    self.show_toast(
                        ToastVariant::Warning,
                        format!(
                            "Discarded AI merge for PR #{number}. \
                             Branch is back where it was before the update."
                        ),
                    );
                }
                UpdatePullRequestOutcome::ConflictsRequireAi { .. } => {
                    self.show_toast(
                        ToastVariant::Warning,
                        "Conflicts found, please resolve them locally or setup `useAi` \
                         setting so we can solve conflicts + merge via AI."
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
                    self.show_toast(
                        ToastVariant::Error,
                        format!(
                            "Failed to abort AI merge for PR #{number}: {}",
                            truncate_error(&detail)
                        ),
                    );
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
                let settings = match self.settings_snapshot() {
                    Ok((config, config_path)) => SettingsScreen::new(config, config_path)
                        .with_global_config_path(global_path)
                        .with_local_config_path(local_path),
                    Err(err) => {
                        let mut settings = SettingsScreen::new(
                            WorktreeConfig::default(),
                            global_config_file().display().to_string(),
                        )
                        .with_global_config_path(global_config_file().display().to_string())
                        .with_local_config_path(local_path);
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
        self.cache = None;
        self.create = None;
        self.delete = None;
        self.settings = None;
        self.setup = None;
        self.setup_project = None;
        self.merge_pr = None;
        self.update_pr = None;
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
                if let Some(config) = self.current_config() {
                    if !config.terminal_command.trim().is_empty() {
                        let _ = open_terminal(&config.terminal_command, &path);
                    }
                }
            }
        }

        self.back_to_menu();
    }

    fn poll_dashboard_updates(&mut self) {
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

        if let Some(screen) = self.dashboard.as_mut() {
            for update in updates_batch {
                if let DashboardUpdate::WithPRs {
                    next_pr_fetch_at, ..
                } = &update
                {
                    screen.set_next_pr_fetch_at(*next_pr_fetch_at);
                }
                screen.set_rows(update.into_rows());
            }
        }
        let has_rows = self
            .dashboard
            .as_ref()
            .is_some_and(DashboardScreen::has_rows);
        for notice in notices {
            if let Some(screen) = self.dashboard.as_mut() {
                if has_rows {
                    screen.set_notice(notice);
                } else {
                    screen.set_error(notice.message);
                }
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
            self.has_local_config(),
            self.current_config()
                .map(|config| !config.worktree_link_patterns.is_empty())
                .unwrap_or(false),
        )
    }

    /// True when the active project already has a `.wisetree.json`. The
    /// menu uses this to decide whether to surface the one-keystroke
    /// "Setup Project Config" entry.
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
            reader
                .load(target_path.parent())
                .map_err(|e| e.to_string())?
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
        let mut state = AppStateService::new();
        state.load();
        let result = check_for_updates(VERSION, &mut state, true).await;
        let _ = tx.send(AppEvent::SettingsUpdateChecked(result));
    });
}

fn kick_off_fetch_opencode_models(tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let result = fetch_opencode_models().await;
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
        let result = service
            .fetch_pr_details(number)
            .await
            .map(|details| MergePrDetailsPayload {
                title: details.title,
                body: details.body,
            })
            .map_err(|err| user_friendly_message(&err));
        let _ = tx.send(AppEvent::MergePrDetailsLoaded(result));
    });
}

fn kick_off_merge_pull_request(
    git_root: Option<String>,
    config: DashboardConfig,
    number: u64,
    subject: String,
    body: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    let Some(root) = git_root.map(PathBuf::from) else {
        let _ = tx.send(AppEvent::MergePrFinished(Err(MergePrFailure {
            number,
            message: "Could not resolve git root for merge.".to_string(),
        })));
        return;
    };
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = match service.merge_pull_request(number, &subject, &body).await {
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
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let base_ref =
            crate::services::dashboard::resolve_base_ref(&PathBuf::from(&worktree_path)).await;
        let _ = tx.send(AppEvent::UpdatePrBaseRefResolved { number, base_ref });
    });
}

fn kick_off_update_branch(
    config: DashboardConfig,
    worktree_path: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        // The mother worktree IS the git root, so reuse the path as the
        // service root — there is no separate "git_root" to resolve from
        // app state for this action.
        let service = DashboardService::new(PathBuf::from(&worktree_path), config);
        let event = match service.update_branch(&worktree_path).await {
            Ok(outcome) => Ok(outcome),
            Err(err) => Err(user_friendly_message(&err)),
        };
        let _ = tx.send(AppEvent::UpdateBranchFinished(event));
    });
}

fn kick_off_commit_and_push(
    git_root: Option<String>,
    config: DashboardConfig,
    request: UpdatePullRequestRequest,
    use_ai: String,
    base_ref: String,
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
    let report_base = base_ref.clone();
    tokio::spawn(async move {
        let service = DashboardService::new(root, config);
        let result = service
            .commit_and_push_ai_merge(&request.worktree_path, &base_ref, &use_ai)
            .await;
        let event = match result {
            Ok(outcome) => Ok(UpdatePrSuccess {
                number,
                base_ref: report_base,
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
        // `base_ref` must be Some here — `handle_update_pr_key` only
        // fires Confirmed after the resolver event populated it. Guard
        // anyway so a race can't blow up the worker.
        let Some(base_ref) = request.base_ref.clone() else {
            let _ = tx.send(AppEvent::UpdatePrFinished(Err(UpdatePrFailure {
                number,
                message: "Base ref was not resolved before confirmation.".to_string(),
            })));
            return;
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
            .update_pull_request_with_progress(&request.worktree_path, &base_ref, Some(progress_tx))
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
            app.git_root = Some("/tmp/repo".into());

            let tx = app_event_tx();
            app.enter_screen(Screen::Settings, &tx);
            for _ in 0..10 {
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

            for _ in 0..10 {
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

            for _ in 0..10 {
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

            for _ in 0..8 {
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

            for _ in 0..8 {
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
                    columns: vec!["branch".into(), "status".into()],
                    use_ai: String::new(),
                    ai_status: Default::default(),
                },
                ..WorktreeConfig::default()
            };
            let local = WorktreeConfig {
                dashboard: DashboardConfig {
                    refresh_interval_ms: 6000,
                    show_pull_requests: false,
                    columns: vec!["branch".into()],
                    use_ai: String::new(),
                    ai_status: Default::default(),
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
                columns: vec![
                    "branch".into(),
                    "status".into(),
                    "ai_status".into(),
                    "pull_request".into(),
                ],
                use_ai: String::new(),
                ai_status: Default::default(),
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
                    columns: vec!["branch".into()],
                    use_ai: String::new(),
                    ai_status: Default::default(),
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
                columns: vec!["branch".into(), "status".into(), "ai_status".into()],
                use_ai: String::new(),
                ai_status: Default::default(),
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
}

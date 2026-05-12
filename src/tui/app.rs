//! `App` — central TUI state machine.
//!
//! Owns screen routing, per-screen async work, and the wrapper-mode selected
//! path handoff used by shell integration.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{env, ffi::OsString};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;
use tokio::sync::mpsc;

use crate::cli::AppMode;
use crate::config::schema::WorktreeConfig;
use crate::config::service::ConfigService;
use crate::constants::{global_config_file, LOCAL_CONFIG_FILE_NAME};
use crate::errors::user_friendly_message;
use crate::files::service::open_terminal;
use crate::git::exec::get_git_root;
use crate::git::service::GitService;
use crate::git::types::{GitBranch, GitWorktree, WorktreeCreateOptions};
use crate::messages::{colors, CREATE_SUCCESS, DELETE_SUCCESS};
use crate::services::{
    check_for_updates, default_dashboard_warning, detect_shell_integration,
    install_shell_integration, resolve_dashboard_columns, AppStateService, DashboardService,
    DashboardWatch, Shell, ShellIntegrationStatus, UpdateCheckResult,
};
use crate::tui::event::{Event, EventLoop};
use crate::tui::router::Screen;
use crate::tui::screens;
use crate::tui::screens::create::{CreateAction, CreateScreen};
use crate::tui::screens::dashboard::{BulkDeleteStatus, DashboardAction, DashboardScreen};
use crate::tui::screens::delete::{
    DeleteAction, DeleteOutcome as ScreenDeleteOutcome, DeleteScreen,
};
use crate::tui::screens::menu::{MenuChoice, MenuOutcome, MenuScreen};
use crate::tui::screens::settings::{CopyDirection, SettingsAction, SettingsScreen};
use crate::tui::screens::setup::{SetupAction, SetupScreen};
use crate::tui::selection::{
    clamp_position, contains_position, extract_text, MouseSelection, SelectionOverlay,
};
use crate::tui::terminal;
use crate::tui::widgets::{render_toast, ToastState, ToastVariant, WelcomeHeader};
use crate::worktree::service::DeleteOutcome as ServiceDeleteOutcome;
use crate::worktree::WorktreeService;
use crate::VERSION;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitPhase {
    Loading,
    Ready,
    Errored,
}

enum AppEvent {
    Initialized(Box<InitOutcome>),
    CreateBranchesLoaded(Result<Vec<GitBranch>, String>),
    CreateFinished(Result<PathBuf, String>),
    DeleteLoaded(Result<Vec<GitWorktree>, String>),
    DeleteFinished(Result<ServiceDeleteOutcome, String>),
    SettingsUpdateChecked(UpdateCheckResult),
    SetupInstalled(Result<ShellIntegrationStatus, String>),
    ClipboardCopyFinished {
        success_message: String,
        error: Option<String>,
    },
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
    create: Option<CreateScreen>,
    delete: Option<DeleteScreen>,
    settings: Option<SettingsScreen>,
    setup: Option<SetupScreen>,
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
            create: None,
            delete: None,
            settings: None,
            setup: None,
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
                Event::Tick => self.tick = self.tick.wrapping_add(1),
                Event::Resize(_, _) => {}
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
            Screen::Create => {
                let h = self
                    .create
                    .as_ref()
                    .map_or(8, |s| s.preferred_content_height());
                let panel = self.render_framed_panel(frame, area, h);
                if let Some(create) = self.create.as_mut() {
                    create.tick = self.tick;
                    create.render(frame, panel);
                }
            }
            Screen::Delete => {
                let h = self
                    .delete
                    .as_ref()
                    .map_or(8, |s| s.preferred_content_height());
                let panel = self.render_framed_panel(frame, area, h);
                if let Some(delete) = self.delete.as_mut() {
                    delete.tick = self.tick;
                    delete.render(frame, panel);
                }
            }
            Screen::Settings => {
                let h = self
                    .settings
                    .as_ref()
                    .map_or(14, |s| s.preferred_content_height());
                let panel = self.render_framed_panel(frame, area, h);
                if let Some(settings) = self.settings.as_mut() {
                    settings.tick = self.tick;
                    settings.render(frame, panel);
                }
            }
            Screen::Setup => {
                let h = self
                    .setup
                    .as_ref()
                    .map_or(8, |s| s.preferred_content_height());
                let panel = self.render_framed_panel(frame, area, h);
                if let Some(setup) = self.setup.as_mut() {
                    setup.tick = self.tick;
                    setup.render(frame, panel);
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
            Screen::Create => self.handle_create_key(key, tx),
            Screen::Delete => self.handle_delete_key(key, tx),
            Screen::Settings => self.handle_settings_key(key, tx),
            Screen::Setup => self.handle_setup_key(key, tx),
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
                    MenuChoice::Setup => self.enter_screen(Screen::Setup, tx),
                    MenuChoice::Create => self.enter_screen(Screen::Create, tx),
                    MenuChoice::Dashboard => self.enter_screen(Screen::Dashboard, tx),
                    MenuChoice::Delete => self.enter_screen(Screen::Delete, tx),
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
        }
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

    fn handle_app_event(&mut self, event: AppEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
        match event {
            AppEvent::Initialized(outcome) => self.apply_init_outcome(*outcome, tx),
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
                            create.set_created_worktree_path(path);
                            self.finish_create_success();
                        }
                        Err(message) => create.set_error(message),
                    }
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
                        } else {
                            if let Some(message) = outcome.branch_delete_error.clone() {
                                self.show_toast(ToastVariant::Warning, message);
                            }
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
        }
    }

    fn apply_init_outcome(&mut self, outcome: InitOutcome, tx: &mpsc::UnboundedSender<AppEvent>) {
        self.git_root = outcome.git_root;
        match outcome.result {
            Ok(service) => {
                self.worktree_service = Some(service);
                self.shell_integration_status = Some(detect_shell_integration());
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
        self.clear_screen_state();
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
                    resolve_dashboard_columns(&config.columns, service.gh_available());
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
                ));
                self.dashboard_watch = Some(service.watch());
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
        }

        if !matches!(screen, Screen::Delete) {
            self.pending_delete_path = None;
            self.pending_bulk_delete_paths.clear();
            self.bulk_delete_queue.clear();
        }
    }

    fn clear_screen_state(&mut self) {
        self.menu = None;
        self.dashboard = None;
        self.dashboard_watch = None;
        self.create = None;
        self.delete = None;
        self.settings = None;
        self.setup = None;
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
        let mut rows_batch = Vec::new();
        let mut notices = Vec::new();
        while let Ok(rows) = watch.rx.try_recv() {
            rows_batch.push(rows);
        }
        while let Ok(notice) = watch.notice_rx.try_recv() {
            notices.push(notice);
        }

        if let Some(screen) = self.dashboard.as_mut() {
            for rows in rows_batch {
                screen.set_rows(rows);
            }
        }
        let has_rows = self
            .dashboard
            .as_ref()
            .is_some_and(DashboardScreen::has_rows);
        for notice in notices {
            if has_rows {
                self.show_toast(ToastVariant::Error, notice);
            } else if let Some(screen) = self.dashboard.as_mut() {
                screen.set_error(notice);
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
        )
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

/// Route SIGHUP (terminal tab closed) and SIGTERM through the normal
/// shutdown path so `restore()` runs and the tty doesn't get stranded in
/// raw mode. Without this, closing the tab with Cmd+W kills the process
/// before cleanup, leaving the parent shell's `dir=$(...)` capture stuck on
/// a tty with ICANON/ECHO disabled.
fn install_termination_listener() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    {
        let flag = flag.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let Ok(mut hup) = signal(SignalKind::hangup()) else {
                return;
            };
            let Ok(mut term) = signal(SignalKind::terminate()) else {
                return;
            };
            tokio::select! {
                _ = hup.recv() => {}
                _ = term.recv() => {}
            }
            flag.store(true, Ordering::Relaxed);
        });
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

        let result = service
            .create_worktree(&options, None)
            .await
            .map(|outcome| outcome.worktree_path)
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

    fn initialized_menu_app() -> App {
        let service = WorktreeService::new(None);

        let mut app = App::new(AppMode::Menu, false);
        app.phase = InitPhase::Ready;
        app.worktree_service = Some(service);
        app.git_root = Some("/tmp/repo".into());
        app.shell_integration_status = Some(ShellIntegrationStatus {
            is_installed: true,
            shell: Shell::Zsh,
            config_path: None,
            reason: None,
        });
        app.menu = Some(app.build_menu_screen());
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

            for _ in 0..5 {
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

            for _ in 0..5 {
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

            for _ in 0..5 {
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

            for _ in 0..6 {
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

            for _ in 0..6 {
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
            create.navigate_after_create = false;
        }

        app.handle_app_event(
            AppEvent::CreateFinished(Ok(PathBuf::from("/tmp/repo/feat-x"))),
            &app_event_tx(),
        );

        assert_eq!(app.screen, Screen::Menu);
        assert!(app.create.is_none());

        let toast = app.toast.current().expect("toast should be shown");
        assert_eq!(toast.variant, ToastVariant::Success);
        assert_eq!(toast.message, CREATE_SUCCESS);

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

        app.handle_app_event(
            AppEvent::CreateFinished(Ok(PathBuf::from("/tmp/repo/feat-x"))),
            &app_event_tx(),
        );

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

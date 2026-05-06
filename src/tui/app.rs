//! `App` — central TUI state machine.
//!
//! Mirrors the responsibilities of branchlet's `App.tsx`: two-phase init,
//! loading splash, error-state with reset-confirm overlay, and dispatch to
//! per-screen renderers. Only the menu / loading / error renderers exist
//! today; future sections plug each interactive screen into `draw`.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use tokio::sync::mpsc;

use crate::cli::AppMode;
use crate::config::service::ConfigService;
use crate::errors::user_friendly_message;
use crate::git::exec::get_git_root;
use crate::tui::event::{Event, EventLoop};
use crate::tui::router::Screen;
use crate::tui::screens;
use crate::tui::screens::list::{ListAction, ListScreen};
use crate::tui::screens::menu::{MenuChoice, MenuOutcome, MenuScreen};
use crate::tui::terminal;
use crate::worktree::WorktreeService;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitPhase {
    /// Resolving git root — render nothing.
    Initializing,
    /// Running `WorktreeService::initialize` — render loading splash.
    Loading,
    /// Initialized; render the active screen.
    Ready,
    /// Initialization failed; render the error screen.
    Errored,
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
    list: Option<ListScreen>,
    /// Wrapper-mode side channel: the path that should be emitted on real
    /// stdout once the TUI tears down. Only set in `is_from_wrapper` mode.
    selected_path: Option<String>,
}

impl App {
    pub fn new(initial_mode: AppMode, is_from_wrapper: bool) -> Self {
        Self {
            screen: Screen::from_mode(initial_mode),
            is_from_wrapper,
            phase: InitPhase::Initializing,
            error: None,
            show_reset_confirm: false,
            last_menu_index: 0,
            tick: 0,
            worktree_service: None,
            git_root: None,
            quit_requested: false,
            menu: None,
            list: None,
            selected_path: None,
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
            let _ = terminal::restore_wrapper_tty();
            let _ = terminal.show_cursor();
            result?;
        } else {
            let mut terminal = terminal::enter()?;
            let result = self.event_loop(&mut terminal).await;
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
        let (init_tx, mut init_rx) = mpsc::unbounded_channel::<InitOutcome>();
        kick_off_initialize(init_tx);

        let mut events = EventLoop::new(Duration::from_millis(50));

        while !self.quit_requested {
            // Drain any completed init outcomes between frames.
            while let Ok(outcome) = init_rx.try_recv() {
                self.apply_init_outcome(outcome);
            }

            terminal.draw(|frame| self.draw(frame))?;

            match events.next_event()? {
                Event::Key(key) => self.handle_key(key),
                Event::Tick => self.tick = self.tick.wrapping_add(1),
                Event::Resize(_, _) | Event::Mouse(_) => {}
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();

        // Bail with a friendlier message rather than panicking on absurdly
        // small terminals.
        if area.width < 20 || area.height < 5 {
            use ratatui::layout::Alignment;
            use ratatui::widgets::Paragraph;
            let msg = Paragraph::new("Terminal too small").alignment(Alignment::Center);
            frame.render_widget(msg, area);
            return;
        }

        match self.phase {
            InitPhase::Initializing => {}
            InitPhase::Loading => {
                screens::loading::draw(frame, area, self.tick, self.screen.as_str());
            }
            InitPhase::Errored => {
                let msg = self.error.as_deref().unwrap_or("Unknown error");
                screens::error::draw(frame, area, msg, self.show_reset_confirm);
            }
            InitPhase::Ready => match self.screen {
                Screen::Menu => {
                    let menu = self.menu.get_or_insert_with(|| {
                        MenuScreen::new(self.last_menu_index, self.git_root.clone(), None)
                    });
                    menu.render(frame, area);
                }
                Screen::List => {
                    let list = self
                        .list
                        .get_or_insert_with(|| ListScreen::new(self.is_from_wrapper, false));
                    list.tick = self.tick;
                    list.render(frame, area);
                }
                _ => {
                    // Other screens land in later sections; placeholder for now.
                    let menu = self.menu.get_or_insert_with(|| {
                        MenuScreen::new(self.last_menu_index, self.git_root.clone(), None)
                    });
                    menu.render(frame, area);
                }
            },
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Global Ctrl+C → quit.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.quit_requested = true;
            return;
        }

        match self.phase {
            InitPhase::Errored => self.handle_error_key(key),
            InitPhase::Ready => self.handle_screen_key(key),
            InitPhase::Initializing | InitPhase::Loading => {}
        }
    }

    fn handle_error_key(&mut self, key: KeyEvent) {
        if self.show_reset_confirm {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.show_reset_confirm = false;
                    match reset_global_config() {
                        Ok(()) => {
                            self.error = None;
                            self.phase = InitPhase::Loading;
                            // Re-kick the init pipeline.
                            let (tx, mut rx) = mpsc::unbounded_channel::<InitOutcome>();
                            kick_off_initialize(tx);
                            // Drain synchronously: by the time the user presses
                            // a key, the join handle should resolve quickly.
                            if let Ok(outcome) = rx.try_recv() {
                                self.apply_init_outcome(outcome);
                            }
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
                self.screen = Screen::Menu;
            }
        }
    }

    fn handle_screen_key(&mut self, key: KeyEvent) {
        match self.screen {
            Screen::Menu => self.handle_menu_key(key),
            Screen::List => self.handle_list_key(key),
            _ => {}
        }
    }

    fn handle_menu_key(&mut self, key: KeyEvent) {
        let menu = self.menu.get_or_insert_with(|| {
            MenuScreen::new(self.last_menu_index, self.git_root.clone(), None)
        });
        match menu.handle_key(key) {
            MenuOutcome::Selected(choice, idx) => {
                self.last_menu_index = idx;
                match choice {
                    MenuChoice::Exit => self.quit_requested = true,
                    MenuChoice::Setup => self.screen = Screen::Setup,
                    MenuChoice::Create => self.screen = Screen::Create,
                    MenuChoice::List => self.screen = Screen::List,
                    MenuChoice::Delete => self.screen = Screen::Delete,
                    MenuChoice::Settings => self.screen = Screen::Settings,
                }
                self.menu = None;
            }
            MenuOutcome::Cancelled => self.quit_requested = true,
            MenuOutcome::Pending => {}
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent) {
        let list = self
            .list
            .get_or_insert_with(|| ListScreen::new(self.is_from_wrapper, false));
        match list.handle_key(key) {
            ListAction::Continue => {}
            ListAction::Back => {
                self.list = None;
                self.screen = Screen::Menu;
            }
            ListAction::NavigateTo(path) => {
                if self.is_from_wrapper {
                    self.selected_path = Some(path);
                }
                self.quit_requested = true;
            }
            ListAction::OpenTerminal(_path) => {
                // Spawning a configured terminal command lands in a later
                // wiring pass — stay on the screen for now.
            }
        }
    }

    fn apply_init_outcome(&mut self, outcome: InitOutcome) {
        self.git_root = outcome.git_root;
        match outcome.result {
            Ok(service) => {
                self.worktree_service = Some(service);
                self.error = None;
                self.phase = InitPhase::Ready;
            }
            Err(message) => {
                self.error = Some(message);
                self.phase = InitPhase::Errored;
            }
        }
    }
}

struct InitOutcome {
    git_root: Option<String>,
    result: Result<WorktreeService, String>,
}

fn kick_off_initialize(tx: mpsc::UnboundedSender<InitOutcome>) {
    tokio::spawn(async move {
        let git_root = get_git_root(None).await;
        let working_dir = git_root.clone().map(std::path::PathBuf::from);
        let mut service = WorktreeService::new(working_dir);
        let result = match service.initialize().await {
            Ok(()) => Ok(service),
            Err(e) => Err(user_friendly_message(&e)),
        };
        let _ = tx.send(InitOutcome { git_root, result });
    });
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
    use crate::git::types::GitWorktree;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ready_app(is_from_wrapper: bool) -> App {
        let mut app = App::new(AppMode::List, is_from_wrapper);
        app.phase = InitPhase::Ready;
        app.screen = Screen::List;
        let mut list = ListScreen::new(is_from_wrapper, false);
        list.set_worktrees(vec![
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
                commit: "cafebabe".into(),
                is_main: false,
                is_clean: true,
                branch_status: None,
            },
        ]);
        app.list = Some(list);
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
        let app = App::new(AppMode::List, true);
        assert!(app.is_from_wrapper);
        assert!(app.selected_path().is_none());
    }

    #[test]
    fn list_navigate_to_in_wrapper_mode_sets_selected_path_and_quits() {
        let mut app = ready_app(true);
        // Enter opens the action menu; second Enter selects "Navigate to
        // Directory" (the only option since terminal_command is unset).
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.selected_path(), Some("/tmp/repo-feat"));
        assert!(app.quit_requested);
    }

    #[test]
    fn list_navigate_to_outside_wrapper_does_not_emit_path() {
        let mut app = ready_app(false);
        app.handle_key(key(KeyCode::Enter));
        // Outside wrapper mode, the Cd option is disabled — Enter on it
        // does not select. App stays alive with no path.
        app.handle_key(key(KeyCode::Enter));
        assert!(app.selected_path().is_none());
        assert!(!app.quit_requested);
    }

    #[test]
    fn list_esc_returns_to_menu_with_no_selected_path() {
        let mut app = ready_app(true);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.selected_path().is_none());
        assert_eq!(app.screen, Screen::Menu);
        assert!(!app.quit_requested);
    }

    #[test]
    fn ctrl_c_quits_without_emitting_path() {
        let mut app = ready_app(true);
        let ctrl_c = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_key(ctrl_c);
        assert!(app.quit_requested);
        assert!(app.selected_path().is_none());
    }
}

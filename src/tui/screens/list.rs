//! List Worktrees screen. Two navigation modes:
//! - `List`: shows the worktrees through the shared `SelectPrompt` widget
//!   in searchable mode (mirroring the Create → Source-branch screen): a
//!   "Search:" row filters the worktrees as the user types, Up/Down move
//!   the cursor inside the filtered view, Enter opens the per-row action
//!   menu for the highlighted worktree, and Esc clears a non-empty query
//!   first or otherwise returns to the menu.
//! - `ActionMenu`: shows "Navigate to Directory" (only enabled when invoked
//!   via the wrapper) and, when configured, "Open with Command".
//!
//! Async work is owned by `App`: it loads worktrees and feeds them into
//! `set_worktrees`/`set_error`, and reacts to `ListAction::OpenTerminal`,
//! `ListAction::NavigateTo`, etc.

use crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::git::types::GitWorktree;
use crate::messages::{
    colors, GIT_ERROR_LIST, LIST_MAIN_INDICATOR, LIST_NO_WORKTREES, LOADING_WORKTREES,
};
use crate::tui::widgets::welcome_header::fold_home;
use crate::tui::widgets::{
    branded_line, SelectOption, SelectOutcome, SelectPrompt, Status, StatusIndicator,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavigationMode {
    List,
    ActionMenu,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListAction {
    Continue,
    Back,
    NavigateTo(String),
    OpenTerminal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionChoice {
    Cd,
    OpenWithCommand,
}

pub struct ListScreen {
    worktrees: Vec<GitWorktree>,
    select: Option<SelectPrompt<usize>>,
    mode: NavigationMode,
    is_from_wrapper: bool,
    has_terminal_command: bool,
    loading: bool,
    error: Option<String>,
    action_select: Option<SelectPrompt<ActionChoice>>,
    // Original-index of the worktree selected when transitioning into the
    // action menu. Captured at transition time because `select.selected`
    // is an index into the *filtered* view when the user has typed a
    // search query, and would otherwise dereference into the wrong row.
    action_target: Option<usize>,
    pub tick: usize,
}

impl ListScreen {
    pub fn new(is_from_wrapper: bool, has_terminal_command: bool) -> Self {
        Self {
            worktrees: Vec::new(),
            select: None,
            mode: NavigationMode::List,
            is_from_wrapper,
            has_terminal_command,
            loading: true,
            error: None,
            action_select: None,
            action_target: None,
            tick: 0,
        }
    }

    pub fn loading(&self) -> bool {
        self.loading
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn worktrees(&self) -> &[GitWorktree] {
        &self.worktrees
    }

    pub fn selected_index(&self) -> usize {
        self.select.as_ref().map(|s| s.selected).unwrap_or(0)
    }

    pub fn set_worktrees(&mut self, worktrees: Vec<GitWorktree>) {
        // Keep the main worktree as the first row so the user can always
        // navigate back to the main checkout from any other worktree.
        let (main, others): (Vec<_>, Vec<_>) = worktrees.into_iter().partition(|w| w.is_main);
        self.worktrees = main.into_iter().chain(others).collect();
        self.select = if self.worktrees.is_empty() {
            None
        } else {
            Some(self.build_main_select())
        };
        self.loading = false;
        self.error = None;
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.loading = false;
    }

    fn build_main_select(&self) -> SelectPrompt<usize> {
        let opts: Vec<SelectOption<usize>> = self
            .worktrees
            .iter()
            .enumerate()
            .map(|(i, wt)| {
                let description = if wt.is_main {
                    format!("{} {LIST_MAIN_INDICATOR}", wt.branch)
                } else {
                    wt.branch.clone()
                };
                SelectOption::new(fold_home(&wt.path), i).with_description(description)
            })
            .collect();
        SelectPrompt::new("Select a worktree:", opts)
            .searchable()
            .without_hint()
    }

    fn build_action_select(&self, _selected: &GitWorktree) -> SelectPrompt<ActionChoice> {
        let mut opts: Vec<SelectOption<ActionChoice>> = Vec::new();
        if self.is_from_wrapper {
            opts.push(SelectOption::new("Navigate to Directory", ActionChoice::Cd));
        } else {
            opts.push(
                SelectOption::new("Navigate to Directory", ActionChoice::Cd)
                    .with_description("requires shell integration")
                    .disabled(),
            );
        }
        if self.has_terminal_command {
            opts.push(
                SelectOption::new("Open with Command", ActionChoice::OpenWithCommand)
                    .with_description("Open using configured terminal command"),
            );
        }
        SelectPrompt::new("Choose action:", opts)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ListAction {
        if self.error.is_some() {
            self.error = None;
            return ListAction::Back;
        }
        if self.loading {
            return ListAction::Continue;
        }
        if self.worktrees.is_empty() {
            return ListAction::Back;
        }

        if matches!(self.mode, NavigationMode::ActionMenu) {
            return self.handle_action_menu(key);
        }

        // The select prompt is searchable, so plain alphanumeric keys feed
        // the filter query (matching the Source-branch screen). Reach the
        // "Open with Command" action via Enter → Action Menu instead.
        let select = match self.select.as_mut() {
            Some(s) => s,
            None => return ListAction::Continue,
        };
        match select.handle_key(key) {
            SelectOutcome::Selected(idx, _) => {
                let wt = self.worktrees[idx].clone();
                self.action_select = Some(self.build_action_select(&wt));
                self.action_target = Some(idx);
                self.mode = NavigationMode::ActionMenu;
                ListAction::Continue
            }
            SelectOutcome::Cancelled => ListAction::Back,
            SelectOutcome::Pending => ListAction::Continue,
        }
    }

    fn handle_action_menu(&mut self, key: KeyEvent) -> ListAction {
        let select = match self.action_select.as_mut() {
            Some(s) => s,
            None => {
                self.mode = NavigationMode::List;
                return ListAction::Continue;
            }
        };
        match select.handle_key(key) {
            SelectOutcome::Selected(_, choice) => {
                let target = self.action_target.unwrap_or_else(|| self.selected_index());
                let path = self.worktrees[target].path.clone();
                self.action_select = None;
                self.action_target = None;
                self.mode = NavigationMode::List;
                match choice {
                    ActionChoice::Cd => {
                        if self.is_from_wrapper {
                            ListAction::NavigateTo(path)
                        } else {
                            ListAction::Continue
                        }
                    }
                    ActionChoice::OpenWithCommand => ListAction::OpenTerminal(path),
                }
            }
            SelectOutcome::Cancelled => {
                self.action_select = None;
                self.action_target = None;
                self.mode = NavigationMode::List;
                ListAction::Continue
            }
            SelectOutcome::Pending => ListAction::Continue,
        }
    }

    /// Inner content height for the framed panel (excludes the rounded
    /// border). Used by `App::render_framed_panel` to size the panel to fit.
    pub fn preferred_content_height(&self) -> u16 {
        if self.loading || self.error.is_some() {
            return 4;
        }
        if matches!(self.mode, NavigationMode::ActionMenu) {
            return 10;
        }
        if self.worktrees.is_empty() {
            return 4;
        }
        // SelectPrompt body: title (1) + spacer (1) + Search row (1) + spacer
        // (1) + visible rows (capped at 10) + optional "more above/below"
        // indicators + custom hint (1) + a row of breathing room.
        let visible = (self.worktrees.len() as u16).min(10);
        let overflow = if self.worktrees.len() > 10 { 2 } else { 0 };
        6 + visible + overflow
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if self.loading {
            StatusIndicator::new(Status::Loading, LOADING_WORKTREES)
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        if let Some(err) = &self.error {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(2)])
                .split(area);
            let err_style = Style::default().fg(colors::ERROR);
            let err_text = format!("{GIT_ERROR_LIST}: {err}");
            frame.render_widget(
                Paragraph::new(Line::from(branded_line(&err_text, err_style))),
                chunks[0],
            );
            frame.render_widget(
                Paragraph::new("Press any key to go back...")
                    .style(Style::default().fg(colors::MUTED)),
                chunks[1],
            );
            return;
        }
        if self.worktrees.is_empty() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Length(2)])
                .split(area);
            let info_style = Style::default().fg(colors::INFO);
            frame.render_widget(
                Paragraph::new(Line::from(branded_line(LIST_NO_WORKTREES, info_style))),
                chunks[0],
            );
            frame.render_widget(
                Paragraph::new("Press any key to go back...")
                    .style(Style::default().fg(colors::MUTED)),
                chunks[1],
            );
            return;
        }

        if matches!(self.mode, NavigationMode::ActionMenu) {
            self.render_action_menu(frame, area);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);

        if let Some(select) = &self.select {
            select.render(frame, chunks[0]);
        }

        let hint = Line::from(vec![
            Span::styled(
                "Type ",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
            Span::styled(
                "to filter  ",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
            Span::styled(
                "↑↓ ",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::BOLD | Modifier::DIM),
            ),
            Span::styled(
                "Navigate  ",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
            Span::styled(
                "↵ ",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::BOLD | Modifier::DIM),
            ),
            Span::styled(
                "Action Menu  ",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
            Span::styled(
                "⎋ ",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::BOLD | Modifier::DIM),
            ),
            Span::styled(
                "Clear / Back",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
        ]);
        frame.render_widget(Paragraph::new(hint), chunks[1]);
    }

    fn render_action_menu(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(area);
        let target = self.action_target.unwrap_or_else(|| self.selected_index());
        let wt = &self.worktrees[target];
        let header = Line::from(vec![
            Span::raw("Selected: "),
            Span::styled(fold_home(&wt.path), Style::default().fg(colors::PRIMARY)),
            Span::raw(" "),
            Span::styled(
                format!("({})", wt.branch),
                Style::default().fg(colors::SUCCESS),
            ),
        ]);
        frame.render_widget(Paragraph::new(header), chunks[0]);
        if let Some(select) = &self.action_select {
            select.render(frame, chunks[1]);
        }
    }
}

//! List Worktrees screen. Two navigation modes:
//! - `List`: shows the worktrees in a two-column table with `➤` cursor; up/
//!   down/jk navigate, numeric 1–9 jumps to that row, Enter opens the
//!   per-row action menu, `e` opens via the configured `terminalCommand`,
//!   Esc returns to the menu.
//! - `ActionMenu`: shows "Navigate to Directory" (only enabled when invoked
//!   via the wrapper) and, when configured, "Open with Command".
//!
//! Async work is owned by `App`: it loads worktrees and feeds them into
//! `set_worktrees`/`set_error`, and reacts to `ListAction::OpenTerminal`,
//! `ListAction::NavigateTo`, etc.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::git::types::GitWorktree;
use crate::messages::{colors, GIT_ERROR_LIST, LIST_NO_WORKTREES, LOADING_WORKTREES};
use crate::tui::widgets::welcome_header::fold_home;
use crate::tui::widgets::{
    SelectOption, SelectOutcome, SelectPrompt, Status, StatusIndicator, SELECT_CURSOR,
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
    selected: usize,
    mode: NavigationMode,
    is_from_wrapper: bool,
    has_terminal_command: bool,
    loading: bool,
    error: Option<String>,
    action_select: Option<SelectPrompt<ActionChoice>>,
    pub tick: usize,
}

impl ListScreen {
    pub fn new(is_from_wrapper: bool, has_terminal_command: bool) -> Self {
        Self {
            worktrees: Vec::new(),
            selected: 0,
            mode: NavigationMode::List,
            is_from_wrapper,
            has_terminal_command,
            loading: true,
            error: None,
            action_select: None,
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
        self.selected
    }

    pub fn set_worktrees(&mut self, worktrees: Vec<GitWorktree>) {
        // Match upstream: drop the main worktree from the list view.
        self.worktrees = worktrees.into_iter().filter(|w| !w.is_main).collect();
        self.selected = 0;
        self.loading = false;
        self.error = None;
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.loading = false;
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

        match key.code {
            KeyCode::Esc => ListAction::Back,
            KeyCode::Up => {
                self.selected = if self.selected == 0 {
                    self.worktrees.len() - 1
                } else {
                    self.selected - 1
                };
                ListAction::Continue
            }
            KeyCode::Down => {
                self.selected = if self.selected + 1 >= self.worktrees.len() {
                    0
                } else {
                    self.selected + 1
                };
                ListAction::Continue
            }
            KeyCode::Enter => {
                let wt = self.worktrees[self.selected].clone();
                self.action_select = Some(self.build_action_select(&wt));
                self.mode = NavigationMode::ActionMenu;
                ListAction::Continue
            }
            KeyCode::Char(c) => match c.to_ascii_lowercase() {
                'k' => {
                    self.selected = if self.selected == 0 {
                        self.worktrees.len() - 1
                    } else {
                        self.selected - 1
                    };
                    ListAction::Continue
                }
                'j' => {
                    self.selected = if self.selected + 1 >= self.worktrees.len() {
                        0
                    } else {
                        self.selected + 1
                    };
                    ListAction::Continue
                }
                'e' => {
                    if self.has_terminal_command {
                        ListAction::OpenTerminal(self.worktrees[self.selected].path.clone())
                    } else {
                        ListAction::Continue
                    }
                }
                d if d.is_ascii_digit() => {
                    if let Some(n) = d.to_digit(10) {
                        if n >= 1 && (n as usize) <= self.worktrees.len().min(9) {
                            self.selected = n as usize - 1;
                        }
                    }
                    ListAction::Continue
                }
                _ => ListAction::Continue,
            },
            _ => ListAction::Continue,
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
                let path = self.worktrees[self.selected].path.clone();
                self.action_select = None;
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
        // header row + N rows + spacer + hint
        4 + self.worktrees.len() as u16
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
            frame.render_widget(
                Paragraph::new(format!("{GIT_ERROR_LIST}: {err}"))
                    .style(Style::default().fg(colors::ERROR)),
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
            frame.render_widget(
                Paragraph::new(LIST_NO_WORKTREES).style(Style::default().fg(colors::INFO)),
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

        let mut constraints = vec![Constraint::Length(1)];
        for _ in &self.worktrees {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Min(0));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let header_row = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(chunks[0]);
        frame.render_widget(
            Paragraph::new("PATH").style(
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::BOLD),
            ),
            header_row[0],
        );
        frame.render_widget(
            Paragraph::new("BRANCH").alignment(Alignment::Right).style(
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::BOLD),
            ),
            header_row[1],
        );

        for (i, wt) in self.worktrees.iter().enumerate() {
            let row_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                .split(chunks[1 + i]);
            let is_selected = i == self.selected;
            let marker = if is_selected { SELECT_CURSOR } else { "  " };
            let path_style = if is_selected {
                Style::default().fg(colors::PRIMARY)
            } else {
                Style::default()
            };
            let path_line = Line::from(vec![
                Span::styled(marker, path_style),
                Span::styled(fold_home(&wt.path), path_style),
            ]);
            frame.render_widget(Paragraph::new(path_line), row_chunks[0]);
            frame.render_widget(
                Paragraph::new(wt.branch.clone())
                    .alignment(Alignment::Right)
                    .style(Style::default().fg(colors::SUCCESS)),
                row_chunks[1],
            );
        }

        let hint = "↑↓/jk Navigate • Enter Action Menu • E Command • Esc Back";
        frame.render_widget(
            Paragraph::new(hint).style(
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
            chunks[1 + self.worktrees.len() + 1],
        );
    }

    fn render_action_menu(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(area);
        let wt = &self.worktrees[self.selected];
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

//! Main menu screen. Renders the welcome header above a `SelectPrompt`
//! styled to match the Wisetree mock-up, with a status bar pinned to the
//! bottom that shows navigation hints, the current version, and the
//! active repository.

use crossterm::event::KeyEvent;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::messages::{
    colors, MENU_CREATE, MENU_DASHBOARD, MENU_EXIT, MENU_SETTINGS, MENU_SETUP, MENU_SETUP_PROJECT,
    MENU_TITLE,
};
use crate::tui::router::Screen;
use crate::tui::widgets::welcome_header::{fold_home, WelcomeHeader};
use crate::tui::widgets::{SelectOption, SelectOutcome, SelectPrompt, SelectStyle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuChoice {
    SetupProject,
    Setup,
    Create,
    Dashboard,
    Settings,
    Exit,
}

pub enum MenuOutcome {
    Selected(MenuChoice, usize),
    Cancelled,
    Pending,
}

pub struct MenuScreen {
    select: SelectPrompt<MenuChoice>,
    git_root: Option<String>,
    has_setup_entry: bool,
    has_setup_project_entry: bool,
}

impl MenuScreen {
    /// `shell_installed = None` means "not yet known" (or unsupported shell)
    /// — the setup entry stays hidden in both cases.
    ///
    /// `has_local_config` controls the new "Setup Project Config" entry: it
    /// renders as the first option only when the project lacks a local
    /// `.wisetree.json` and we know which repository we're in (so the user
    /// can bootstrap their three preset lists with a single keystroke).
    pub fn new(
        default_index: usize,
        git_root: Option<String>,
        shell_installed: Option<bool>,
        has_local_config: bool,
    ) -> Self {
        let has_setup_project_entry = git_root.is_some() && !has_local_config;
        let has_setup_entry = matches!(shell_installed, Some(false));
        let mut options: Vec<SelectOption<MenuChoice>> = Vec::new();
        if has_setup_project_entry {
            options.push(
                SelectOption::new(MENU_SETUP_PROJECT, MenuChoice::SetupProject)
                    .with_color(colors::WARNING)
                    .with_description("recommended"),
            );
        }
        if has_setup_entry {
            options.push(
                SelectOption::new(MENU_SETUP, MenuChoice::Setup)
                    .with_color(colors::WARNING)
                    .with_description("recommended"),
            );
        }
        options.push(SelectOption::new(MENU_CREATE, MenuChoice::Create));
        options.push(SelectOption::new(MENU_DASHBOARD, MenuChoice::Dashboard));
        options.push(SelectOption::new(MENU_SETTINGS, MenuChoice::Settings));
        options.push(SelectOption::new(MENU_EXIT, MenuChoice::Exit));

        let select = SelectPrompt::new(MENU_TITLE, options)
            .with_default_index(default_index)
            .with_style(SelectStyle::Boxed)
            .without_hint();
        Self {
            select,
            git_root,
            has_setup_entry,
            has_setup_project_entry,
        }
    }

    pub fn selected_index(&self) -> usize {
        self.select.selected
    }

    pub fn has_setup_entry(&self) -> bool {
        self.has_setup_entry
    }

    pub fn has_setup_project_entry(&self) -> bool {
        self.has_setup_project_entry
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> MenuOutcome {
        match self.select.handle_key(key) {
            SelectOutcome::Selected(idx, choice) => MenuOutcome::Selected(choice, idx),
            SelectOutcome::Cancelled => MenuOutcome::Cancelled,
            SelectOutcome::Pending => MenuOutcome::Pending,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let select_height = self.select_panel_height();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Length(select_height),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(area);

        let cwd = self.git_root.as_deref().unwrap_or("");
        let header = WelcomeHeader::new(Screen::Menu, cwd);
        header.render(frame, chunks[0]);

        self.select.render(frame, chunks[1]);

        render_status_bar(frame, chunks[2], cwd);
    }

    /// Total rows the menu screen wants — header + select panel + status bar.
    /// Excludes any trailing filler.
    pub fn preferred_height(&self) -> u16 {
        4 + self.select_panel_height() + 1
    }

    /// Inner height of the boxed select panel: borders (2) + title (1) +
    /// spacer (1) + N option rows + breathing room (1).
    fn select_panel_height(&self) -> u16 {
        (self.select.options.len() as u16).saturating_add(5)
    }
}

fn render_status_bar(frame: &mut Frame, area: Rect, cwd: &str) {
    let status_style = Style::default().bg(colors::STATUS_BG);
    frame.render_widget(Paragraph::new("").style(status_style), area);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let hints = Line::from(vec![
        Span::styled(
            "{↑}{↓} ",
            Style::default()
                .fg(colors::STATUS_TEXT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Nav  ", Style::default().fg(colors::MENU_TEXT)),
        Span::styled(
            "↵ ",
            Style::default()
                .fg(colors::STATUS_TEXT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Sel  ", Style::default().fg(colors::MENU_TEXT)),
        Span::styled(
            "⎋ ",
            Style::default()
                .fg(colors::STATUS_TEXT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Esc", Style::default().fg(colors::MENU_TEXT)),
    ]);
    frame.render_widget(Paragraph::new(hints).style(status_style), columns[0]);

    let repo_name = repo_basename(cwd);
    let right = Line::from(vec![
        Span::styled(
            format!("Version {}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(colors::HEADER_SUBTITLE),
        ),
        Span::styled(" | ", Style::default().fg(colors::MUTED)),
        Span::styled(
            "Active Repo: ",
            Style::default().fg(colors::HEADER_SUBTITLE),
        ),
        Span::styled(
            repo_name,
            Style::default()
                .fg(colors::MENU_TEXT)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(right)
            .alignment(Alignment::Right)
            .style(status_style),
        columns[1],
    );
}

fn repo_basename(cwd: &str) -> String {
    if cwd.is_empty() {
        return "—".to_string();
    }
    let folded = fold_home(cwd);
    folded
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("—")
        .to_string()
}

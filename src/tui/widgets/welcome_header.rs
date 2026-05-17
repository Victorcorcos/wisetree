//! Bordered "🧙 Welcome to Wisetree" header with the current repository
//! cwd printed beneath. The cwd is folded against `$HOME` so e.g.
//! `/Users/me/code` renders as `~/code`.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::messages::colors;
use crate::tui::router::Screen;

pub struct WelcomeHeader<'a> {
    pub screen: Screen,
    pub cwd: &'a str,
}

impl<'a> WelcomeHeader<'a> {
    pub fn new(screen: Screen, cwd: &'a str) -> Self {
        Self { screen, cwd }
    }

    fn mode_label(&self) -> Option<&'static str> {
        match self.screen {
            Screen::Menu => None,
            Screen::Create => Some("Create"),
            Screen::Dashboard => Some("Dashboard"),
            Screen::Delete => Some("Delete"),
            Screen::MergePullRequest => Some("Merge Pull Request"),
            Screen::Settings => Some("Settings"),
            Screen::Setup => Some("Setup"),
            Screen::SetupProject => Some("Setup Project Config"),
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let panel_style = Style::default().bg(colors::HEADER_BG);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(
                Style::default()
                    .fg(colors::HEADER_BORDER)
                    .bg(colors::HEADER_BG),
            )
            .style(panel_style);

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(inner);

        let title_style = Style::default()
            .fg(colors::HEADER_TITLE)
            .bg(colors::HEADER_BG)
            .add_modifier(Modifier::BOLD);
        let brand_style = Style::default()
            .fg(colors::BRAND)
            .bg(colors::HEADER_BG)
            .add_modifier(Modifier::BOLD);
        let mut header_spans: Vec<Span> = vec![
            Span::styled("  ", panel_style),
            Span::styled("🧙 ", panel_style),
        ];
        match self.mode_label() {
            None => {
                header_spans.push(Span::styled("Welcome to ", title_style));
                header_spans.push(Span::styled("Wisetree", brand_style));
            }
            Some(label) => {
                header_spans.push(Span::styled("Wisetree", brand_style));
                header_spans.push(Span::styled(format!(" - {label}"), title_style));
            }
        }
        let header_line = Line::from(header_spans);
        frame.render_widget(Paragraph::new(header_line).style(panel_style), chunks[0]);

        let subtitle = Line::from(vec![
            Span::styled("  ", panel_style),
            Span::styled(
                "Current Repository",
                Style::default()
                    .fg(colors::HEADER_SUBTITLE)
                    .bg(colors::HEADER_BG),
            ),
            Span::styled(
                " | ",
                Style::default().fg(colors::MUTED).bg(colors::HEADER_BG),
            ),
            Span::styled(
                "cwd: ",
                Style::default()
                    .fg(colors::HEADER_SUBTITLE)
                    .bg(colors::HEADER_BG),
            ),
            Span::styled(
                fold_home(self.cwd),
                Style::default().fg(colors::MENU_TEXT).bg(colors::HEADER_BG),
            ),
        ]);
        frame.render_widget(Paragraph::new(subtitle).style(panel_style), chunks[1]);
    }
}

pub fn fold_home(path: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_string();
    };
    let home = home.to_string_lossy();
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".to_string();
    }
    let prefix = if home.ends_with('/') {
        home.to_string()
    } else {
        format!("{home}/")
    };
    if let Some(rest) = path.strip_prefix(prefix.as_str()) {
        format!("~/{rest}")
    } else {
        path.to_string()
    }
}

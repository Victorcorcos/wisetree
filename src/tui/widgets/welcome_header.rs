//! Bordered "🌳 Welcome to Wisetree!" / "🌳 Wisetree - <mode>" header with
//! the current working directory printed beneath. The cwd is folded against
//! `$HOME` so e.g. `/Users/me/code` renders as `~/code`.

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
            Screen::List => Some("List"),
            Screen::Delete => Some("Delete"),
            Screen::Settings => Some("Settings"),
            Screen::Setup => Some("Setup"),
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::MUTED));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(inner);

        let header_line = match self.mode_label() {
            None => Line::from(vec![
                Span::raw("🌳 Welcome to "),
                Span::styled(
                    "Wisetree",
                    Style::default()
                        .fg(colors::PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("!"),
            ]),
            Some(label) => Line::from(vec![
                Span::raw("🌳 Wisetree - "),
                Span::styled(
                    label,
                    Style::default()
                        .fg(colors::PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
        };
        frame.render_widget(Paragraph::new(header_line), chunks[0]);

        let cwd_text = format!("cwd: {}", fold_home(self.cwd));
        frame.render_widget(
            Paragraph::new(cwd_text).style(Style::default().fg(colors::MUTED)),
            chunks[1],
        );
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

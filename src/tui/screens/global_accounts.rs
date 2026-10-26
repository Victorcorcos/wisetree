//! Account selection for the shared Global Dashboard. App owns discovery and saving.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::messages::{self, colors};
use crate::services::global_dashboard::github::{AccountDiscovery, GitHubError};
use crate::tui::screens::loading::spinner_frames;
use crate::tui::widgets::{SelectOption, SelectOutcome, SelectPrompt};

#[derive(Debug, PartialEq, Eq)]
pub enum GlobalAccountsAction {
    Continue,
    Back,
    Refresh,
    Open(Vec<String>),
}

pub struct GlobalAccountsScreen {
    select: SelectPrompt<String>,
    selected_accounts: Vec<String>,
    loading: bool,
    error: Option<String>,
    warning: Option<&'static str>,
    pub tick: usize,
}

impl GlobalAccountsScreen {
    pub fn new(saved_accounts: Vec<String>) -> Self {
        let mut screen = Self {
            select: SelectPrompt::new(messages::GLOBAL_ACCOUNTS_TITLE, Vec::new())
                .searchable()
                .without_hint(),
            selected_accounts: Vec::new(),
            loading: true,
            error: None,
            warning: None,
            tick: 0,
        };
        for login in saved_accounts {
            if !screen.is_selected(&login) {
                screen.selected_accounts.push(login.clone());
                screen.add_account(login);
            }
        }
        screen
    }

    pub fn selected_accounts(&self) -> Vec<String> {
        self.selected_accounts.clone()
    }

    fn is_selected(&self, login: &str) -> bool {
        self.selected_accounts
            .iter()
            .any(|selected| selected.eq_ignore_ascii_case(login))
    }

    fn add_account(&mut self, login: String) {
        if !self
            .select
            .options
            .iter()
            .any(|option| option.value.eq_ignore_ascii_case(&login))
        {
            self.select.options.push(SelectOption::new("", login));
        }
        self.update_labels();
    }

    fn update_labels(&mut self) {
        for option in &mut self.select.options {
            let checked = self
                .selected_accounts
                .iter()
                .any(|login| login.eq_ignore_ascii_case(&option.value));
            option.label = format!("{} {}", if checked { "☒" } else { "☐" }, option.value);
        }
    }

    pub fn start_loading(&mut self) {
        self.loading = true;
        self.error = None;
        self.warning = None;
    }

    pub fn set_discovery(&mut self, result: Result<AccountDiscovery, GitHubError>) {
        self.loading = false;
        match result {
            Ok(discovery) => {
                for account in discovery.accounts {
                    self.add_account(account.login);
                }
                self.error = (!discovery.warnings.is_empty()).then(|| {
                    discovery
                        .warnings
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                });
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> GlobalAccountsAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            return GlobalAccountsAction::Refresh;
        }
        if key.code == KeyCode::Enter {
            if self.loading {
                return GlobalAccountsAction::Continue;
            }
            if self.selected_accounts.is_empty() {
                self.warning = Some(messages::GLOBAL_ACCOUNTS_REQUIRED);
                return GlobalAccountsAction::Continue;
            }
            return GlobalAccountsAction::Open(self.selected_accounts());
        }
        if key.code == KeyCode::Char(' ') {
            if let Some(&index) = self.select.filtered_indices().get(self.select.selected) {
                let login = self.select.options[index].value.clone();
                if self.is_selected(&login) {
                    self.selected_accounts
                        .retain(|selected| !selected.eq_ignore_ascii_case(&login));
                } else {
                    self.selected_accounts.push(login);
                }
                self.update_labels();
                self.warning = None;
            }
            return GlobalAccountsAction::Continue;
        }
        match self.select.handle_key(key) {
            SelectOutcome::Cancelled => GlobalAccountsAction::Back,
            _ => GlobalAccountsAction::Continue,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Length(u16::from(self.loading)),
            Constraint::Min(0),
            Constraint::Length(if self.error.is_some() { 3 } else { 0 }),
            Constraint::Length(u16::from(self.warning.is_some())),
            Constraint::Length(1),
        ])
        .split(area);
        if self.loading {
            let spinner = spinner_frames()[self.tick % spinner_frames().len()];
            frame.render_widget(
                Paragraph::new(format!("{spinner} {}", messages::GLOBAL_ACCOUNTS_LOADING))
                    .style(Style::default().fg(colors::INFO)),
                chunks[0],
            );
        }
        self.select.render(frame, chunks[1]);
        if let Some(error) = &self.error {
            frame.render_widget(
                Paragraph::new(error.as_str())
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(colors::ERROR)),
                chunks[2],
            );
        }
        if let Some(warning) = self.warning {
            frame.render_widget(
                Paragraph::new(warning).style(Style::default().fg(colors::WARNING)),
                chunks[3],
            );
        }
        frame.render_widget(
            Paragraph::new(messages::GLOBAL_ACCOUNTS_HINT).style(
                Style::default()
                    .fg(colors::STATUS_TEXT)
                    .bg(colors::STATUS_BG),
            ),
            chunks[4],
        );
    }
}

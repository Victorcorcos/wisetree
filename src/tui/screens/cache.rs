//! Shared cache inspection screen.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::files::{CacheEntryInfo, CacheOverview};
use crate::messages::colors;
use crate::tui::widgets::{
    branded_line, ConfirmChoice, ConfirmDialog, ConfirmOutcome, ConfirmVariant, SelectOption,
    SelectOutcome, SelectPrompt, Status, StatusIndicator,
};

const CACHE_LOADING: &str = "Loading shared cache...";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheAction {
    Continue,
    Back,
    Refresh,
    DeleteEntry(String),
}

pub struct CacheScreen {
    overview: Option<CacheOverview>,
    select: Option<SelectPrompt<String>>,
    confirm: Option<ConfirmDialog>,
    pending_delete: Option<String>,
    loading: bool,
    error: Option<String>,
    pub tick: usize,
}

impl CacheScreen {
    pub fn new() -> Self {
        Self {
            overview: None,
            select: None,
            confirm: None,
            pending_delete: None,
            loading: true,
            error: None,
            tick: 0,
        }
    }

    pub fn set_overview(&mut self, overview: CacheOverview) {
        self.select = if overview.entries.is_empty() {
            None
        } else {
            Some(build_select(&overview))
        };
        self.overview = Some(overview);
        self.confirm = None;
        self.pending_delete = None;
        self.loading = false;
        self.error = None;
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.loading = false;
    }

    pub fn start_loading(&mut self) {
        self.loading = true;
        self.confirm = None;
        self.pending_delete = None;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> CacheAction {
        if self.error.is_some() {
            self.error = None;
            return CacheAction::Back;
        }
        if self.loading {
            return CacheAction::Continue;
        }

        if let Some(confirm) = self.confirm.as_mut() {
            return match confirm.handle_key(key) {
                ConfirmOutcome::Confirmed => {
                    let target = self.pending_delete.take().unwrap_or_default();
                    self.confirm = None;
                    CacheAction::DeleteEntry(target)
                }
                ConfirmOutcome::Declined | ConfirmOutcome::Cancelled => {
                    self.confirm = None;
                    self.pending_delete = None;
                    CacheAction::Continue
                }
                ConfirmOutcome::Pending => CacheAction::Continue,
            };
        }

        if self
            .overview
            .as_ref()
            .is_some_and(|overview| overview.entries.is_empty())
        {
            return CacheAction::Back;
        }

        if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) {
            return CacheAction::Refresh;
        }

        if matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D')) {
            if let Some(entry) = self.selected_entry_info() {
                let relative_path = entry.relative_path.clone();
                let user_count = entry.user_count;
                self.pending_delete = Some(relative_path.clone());
                let warning = if user_count == 0 {
                    "No active worktrees currently reference this entry.".to_string()
                } else {
                    format!(
                        "Warning: {} active worktree(s) still reference this entry.",
                        user_count
                    )
                };
                self.confirm = Some(
                    ConfirmDialog::new(
                        "Delete Cache Entry",
                        format!(
                            "Delete shared cache entry '{}' ?\n\n{}\nThis does not delete any worktree directories, but linked worktrees may stop working until the cache is rebuilt.",
                            relative_path, warning
                        ),
                    )
                    .with_variant(ConfirmVariant::Danger)
                    .with_default(ConfirmChoice::Cancel),
                );
            }
            return CacheAction::Continue;
        }

        let Some(select) = self.select.as_mut() else {
            return CacheAction::Back;
        };
        match select.handle_key(key) {
            SelectOutcome::Cancelled => CacheAction::Back,
            SelectOutcome::Selected(_, _) | SelectOutcome::Pending => CacheAction::Continue,
        }
    }

    pub fn preferred_content_height(&self) -> u16 {
        if self.loading || self.error.is_some() {
            return 4;
        }

        let Some(overview) = self.overview.as_ref() else {
            return 4;
        };

        if overview.entries.is_empty() {
            return 5;
        }

        let selected_users = self
            .selected_entry_info()
            .map(|entry| entry.users.len() as u16)
            .unwrap_or(0)
            .min(3);
        11 + (overview.entries.len() as u16).min(10) + selected_users
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if self.loading {
            StatusIndicator::new(Status::Loading, CACHE_LOADING)
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        if let Some(error) = &self.error {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(2)])
                .split(area);
            frame.render_widget(
                Paragraph::new(Line::from(branded_line(
                    error,
                    Style::default().fg(colors::ERROR),
                ))),
                chunks[0],
            );
            frame.render_widget(
                Paragraph::new("Press any key to go back...")
                    .style(Style::default().fg(colors::MUTED)),
                chunks[1],
            );
            return;
        }

        let Some(overview) = self.overview.as_ref() else {
            return;
        };

        if overview.entries.is_empty() {
            let lines = vec![
                Line::from(branded_line(
                    "Shared Cache",
                    Style::default().fg(colors::INFO).add_modifier(Modifier::BOLD),
                )),
                Line::from(branded_line(
                    &format!("Cache root: {}", overview.cache_dir.display()),
                    Style::default().fg(colors::EMPHASIS),
                )),
                Line::from(branded_line(
                    "No cache entries yet. Create a worktree with link patterns to populate the shared cache.",
                    Style::default().fg(colors::MUTED),
                )),
                Line::from(branded_line(
                    "Press any key to go back.",
                    Style::default().fg(colors::MUTED).add_modifier(Modifier::DIM),
                )),
            ];
            frame.render_widget(Paragraph::new(lines), area);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(
                    5 + self
                        .selected_entry_info()
                        .map(|entry| entry.users.len() as u16)
                        .unwrap_or(0)
                        .min(3),
                ),
                Constraint::Length(1),
            ])
            .split(area);

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(branded_line(
                    "Shared Cache",
                    Style::default()
                        .fg(colors::INFO)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(vec![
                    Span::styled("Root: ", Style::default().fg(colors::MUTED)),
                    Span::styled(
                        overview.cache_dir.display().to_string(),
                        Style::default().fg(colors::EMPHASIS),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Total size: ", Style::default().fg(colors::MUTED)),
                    Span::styled(
                        human_size(overview.total_size_bytes),
                        Style::default().fg(colors::SUCCESS),
                    ),
                    Span::styled("  Active worktrees: ", Style::default().fg(colors::MUTED)),
                    Span::styled(
                        overview.users.len().to_string(),
                        Style::default().fg(colors::EMPHASIS),
                    ),
                ]),
            ]),
            chunks[0],
        );

        if let Some(select) = &self.select {
            select.render(frame, chunks[1]);
        }
        self.render_selected_entry(frame, chunks[2]);
        frame.render_widget(
            Paragraph::new("Type to filter • d Delete entry • r Refresh • Esc Back").style(
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
            chunks[3],
        );

        if let Some(confirm) = &self.confirm {
            confirm.render(frame, chunks[1]);
        }
    }

    fn render_selected_entry(&self, frame: &mut Frame, area: Rect) {
        let Some(entry) = self.selected_entry_info() else {
            return;
        };

        let mut lines = vec![
            Line::from(branded_line(
                "Selected Entry",
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("Pattern: ", Style::default().fg(colors::MUTED)),
                Span::styled(
                    entry.relative_path.clone(),
                    Style::default().fg(colors::EMPHASIS),
                ),
            ]),
            Line::from(vec![
                Span::styled("Cache path: ", Style::default().fg(colors::MUTED)),
                Span::styled(
                    entry.path.display().to_string(),
                    Style::default().fg(colors::EMPHASIS),
                ),
            ]),
            Line::from(vec![
                Span::styled("Size: ", Style::default().fg(colors::MUTED)),
                Span::styled(
                    human_size(entry.size_bytes),
                    Style::default().fg(colors::SUCCESS),
                ),
                Span::styled("  Age: ", Style::default().fg(colors::MUTED)),
                Span::styled(
                    format!("{}d", entry.age_days),
                    Style::default().fg(colors::EMPHASIS),
                ),
                Span::styled("  Active users: ", Style::default().fg(colors::MUTED)),
                Span::styled(
                    entry.user_count.to_string(),
                    Style::default().fg(colors::INFO),
                ),
            ]),
        ];

        if entry.users.is_empty() {
            lines.push(Line::from(branded_line(
                "No active worktrees currently reference this cache entry.",
                Style::default().fg(colors::WARNING),
            )));
        } else {
            for user in entry.users.iter().take(3) {
                lines.push(Line::from(vec![
                    Span::styled("  • ", Style::default().fg(colors::ACCENT)),
                    Span::styled(
                        user.worktree_path.clone(),
                        Style::default().fg(colors::EMPHASIS),
                    ),
                ]));
            }
            if entry.users.len() > 3 {
                lines.push(Line::from(branded_line(
                    &format!("  • {} more active worktree(s)", entry.users.len() - 3),
                    Style::default().fg(colors::MUTED),
                )));
            }
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn selected_entry_info(&self) -> Option<&CacheEntryInfo> {
        let overview = self.overview.as_ref()?;
        let select = self.select.as_ref()?;
        let filtered = if select.searchable && !select.query.is_empty() {
            let query = select.query.to_lowercase();
            select
                .options
                .iter()
                .enumerate()
                .filter_map(|(idx, option)| {
                    option.label.to_lowercase().contains(&query).then_some(idx)
                })
                .collect::<Vec<_>>()
        } else {
            (0..select.options.len()).collect::<Vec<_>>()
        };
        let original = *filtered.get(select.selected)?;
        let selected_pattern = &select.options.get(original)?.value;
        overview
            .entries
            .iter()
            .find(|entry| &entry.relative_path == selected_pattern)
    }
}

impl Default for CacheScreen {
    fn default() -> Self {
        Self::new()
    }
}

fn build_select(overview: &CacheOverview) -> SelectPrompt<String> {
    let options = overview
        .entries
        .iter()
        .map(|entry| {
            SelectOption::new(entry.relative_path.clone(), entry.relative_path.clone())
                .with_description(format!(
                    "{} • {} users • {}d old",
                    human_size(entry.size_bytes),
                    entry.user_count,
                    entry.age_days
                ))
        })
        .collect();
    SelectPrompt::new("Select a cache entry:", options)
        .searchable()
        .without_hint()
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

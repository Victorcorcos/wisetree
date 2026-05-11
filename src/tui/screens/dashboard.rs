//! Live dashboard screen.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::messages::colors;
use crate::services::{DashboardRow, PrState};
use crate::tui::widgets::welcome_header::fold_home;
use crate::tui::widgets::{
    SelectOption, SelectOutcome, SelectPrompt, Status, StatusIndicator,
};

const MAX_VISIBLE_ROWS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardMode {
    Table,
    ActionMenu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionChoice {
    Navigate,
    OpenWithCommand,
    CopyPath,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardAction {
    Continue,
    Back,
    Refresh,
    NavigateTo(String),
    OpenTerminal(String),
    JumpToDelete(String),
    CopyPath(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardColumn {
    Branch,
    Status,
    AheadBehind,
    LastCommit,
    PullRequest,
}

struct DashboardTableLayout {
    worktree_width: u16,
    visible_columns: Vec<DashboardColumn>,
    hidden_columns: Vec<DashboardColumn>,
    compact: bool,
}

pub struct DashboardScreen {
    rows: Vec<DashboardRow>,
    selected: usize,
    loading: bool,
    error: Option<String>,
    mode: DashboardMode,
    query: String,
    action_select: Option<SelectPrompt<ActionChoice>>,
    action_target: Option<usize>,
    is_from_wrapper: bool,
    has_terminal_command: bool,
    has_clipboard: bool,
    columns: Vec<DashboardColumn>,
    warnings: Vec<String>,
    notice: Option<String>,
    refreshed_at: Option<Instant>,
    pub tick: usize,
}

impl DashboardScreen {
    pub fn new(
        is_from_wrapper: bool,
        has_terminal_command: bool,
        has_clipboard: bool,
        columns: Vec<String>,
        warnings: Vec<String>,
    ) -> Self {
        Self {
            rows: Vec::new(),
            selected: 0,
            loading: true,
            error: None,
            mode: DashboardMode::Table,
            query: String::new(),
            action_select: None,
            action_target: None,
            is_from_wrapper,
            has_terminal_command,
            has_clipboard,
            columns: columns
                .into_iter()
                .filter_map(|column| DashboardColumn::parse(&column))
                .collect(),
            warnings,
            notice: None,
            refreshed_at: None,
            tick: 0,
        }
    }

    pub fn set_rows(&mut self, rows: Vec<DashboardRow>) {
        self.rows = rows;
        self.loading = false;
        self.error = None;
        self.notice = None;
        self.refreshed_at = Some(Instant::now());
        let filtered_len = self.filtered_indices().len();
        if filtered_len == 0 {
            self.selected = 0;
        } else if self.selected >= filtered_len {
            self.selected = filtered_len - 1;
        }
    }

    pub fn set_notice(&mut self, notice: String) {
        self.notice = Some(notice);
    }

    pub fn set_error(&mut self, error: String) {
        self.loading = false;
        self.error = Some(error);
    }

    pub fn has_rows(&self) -> bool {
        !self.rows.is_empty()
    }

    pub fn preferred_content_height(&self) -> u16 {
        if self.loading {
            return 4;
        }
        if self.error.is_some() && self.rows.is_empty() {
            return 5;
        }
        if matches!(self.mode, DashboardMode::ActionMenu) {
            return 11;
        }
        let visible = self.filtered_indices().len().min(MAX_VISIBLE_ROWS) as u16;
        let overflow = if self.filtered_indices().len() > MAX_VISIBLE_ROWS {
            2
        } else {
            0
        };
        let table_rows = visible.max(1);
        // Search bar (1 line) plus a blank spacer above and below it (2 lines).
        5 + 3 + table_rows + overflow + 4
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DashboardAction {
        if self.loading {
            return DashboardAction::Continue;
        }

        if self.error.is_some() && self.rows.is_empty() {
            return match key.code {
                KeyCode::Char('r') | KeyCode::Char('R')
                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    DashboardAction::Refresh
                }
                KeyCode::Esc => DashboardAction::Back,
                _ => DashboardAction::Continue,
            };
        }

        if matches!(self.mode, DashboardMode::ActionMenu) {
            return self.handle_action_menu(key);
        }

        // Refresh shortcut (Ctrl+R) takes priority so it isn't swallowed by
        // the always-on search input.
        if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return DashboardAction::Refresh;
        }

        match key.code {
            KeyCode::Esc => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.selected = 0;
                    DashboardAction::Continue
                } else {
                    DashboardAction::Back
                }
            }
            KeyCode::Up => {
                self.move_selection(-1);
                DashboardAction::Continue
            }
            KeyCode::Down => {
                self.move_selection(1);
                DashboardAction::Continue
            }
            KeyCode::Enter => {
                let Some(index) = self.selected_row_index() else {
                    return DashboardAction::Continue;
                };
                let row = self.rows[index].clone();
                self.action_select = Some(self.build_action_select(&row));
                self.action_target = Some(index);
                self.mode = DashboardMode::ActionMenu;
                DashboardAction::Continue
            }
            KeyCode::Backspace | KeyCode::Delete => {
                if self.query.pop().is_some() {
                    self.selected = 0;
                }
                DashboardAction::Continue
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(c);
                self.selected = 0;
                DashboardAction::Continue
            }
            _ => DashboardAction::Continue,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if self.loading {
            StatusIndicator::new(Status::Loading, "Loading dashboard...")
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        if let Some(error) = &self.error {
            if self.rows.is_empty() {
                self.render_error(frame, area, error);
                return;
            }
        }

        if matches!(self.mode, DashboardMode::ActionMenu) {
            self.render_action_menu(frame, area);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // status banner
                Constraint::Length(1), // spacer above search
                Constraint::Length(1), // search line
                Constraint::Length(1), // spacer below search
                Constraint::Min(4),    // table
                Constraint::Length(4), // footer
            ])
            .split(area);

        frame.render_widget(Paragraph::new(self.status_banner()), chunks[0]);
        frame.render_widget(Paragraph::new(self.search_line()), chunks[2]);
        let layout = self.table_layout(chunks[4].width);
        self.render_table(frame, chunks[4], &layout);
        frame.render_widget(
            Paragraph::new(self.footer_lines(chunks[4].width, &layout)),
            chunks[5],
        );
    }

    fn build_action_select(&self, row: &DashboardRow) -> SelectPrompt<ActionChoice> {
        let mut options = Vec::new();
        if self.is_from_wrapper {
            options.push(SelectOption::new(
                "Navigate to Directory",
                ActionChoice::Navigate,
            ));
        }
        if self.has_terminal_command {
            options.push(
                SelectOption::new("Open with Command", ActionChoice::OpenWithCommand)
                    .with_description("Open using configured terminal command"),
            );
        }
        // Delete is intentionally placed above Copy path: from the dashboard
        // the user is more likely to want to delete the selected worktree than
        // to copy its path.
        options.push(
            SelectOption::new("Delete this worktree", ActionChoice::Delete).with_description(
                format!(
                    "{} ({})",
                    fold_home(&row.worktree.path),
                    row.worktree.branch
                ),
            ),
        );
        if self.has_clipboard {
            options.push(SelectOption::new(
                "Copy path to clipboard",
                ActionChoice::CopyPath,
            ));
        }
        SelectPrompt::new("Choose action:", options).without_hint()
    }

    fn handle_action_menu(&mut self, key: KeyEvent) -> DashboardAction {
        let Some(select) = self.action_select.as_mut() else {
            self.mode = DashboardMode::Table;
            return DashboardAction::Continue;
        };
        match select.handle_key(key) {
            SelectOutcome::Selected(_, choice) => {
                let Some(index) = self.action_target else {
                    self.mode = DashboardMode::Table;
                    self.action_select = None;
                    return DashboardAction::Continue;
                };
                let path = self.rows[index].worktree.path.clone();
                self.mode = DashboardMode::Table;
                self.action_select = None;
                self.action_target = None;
                match choice {
                    ActionChoice::Navigate => DashboardAction::NavigateTo(path),
                    ActionChoice::OpenWithCommand => DashboardAction::OpenTerminal(path),
                    ActionChoice::CopyPath => DashboardAction::CopyPath(path),
                    ActionChoice::Delete => DashboardAction::JumpToDelete(path),
                }
            }
            SelectOutcome::Cancelled => {
                self.mode = DashboardMode::Table;
                self.action_select = None;
                self.action_target = None;
                DashboardAction::Continue
            }
            SelectOutcome::Pending => DashboardAction::Continue,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let filtered_len = self.filtered_indices().len();
        if filtered_len == 0 {
            self.selected = 0;
            return;
        }
        if delta < 0 {
            self.selected = if self.selected == 0 {
                filtered_len - 1
            } else {
                self.selected - 1
            };
        } else {
            self.selected = if self.selected + 1 >= filtered_len {
                0
            } else {
                self.selected + 1
            };
        }
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let query = self.query.trim().to_ascii_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                if query.is_empty() || self.row_matches_query(row, &query) {
                    Some(index)
                } else {
                    None
                }
            })
            .collect()
    }

    fn row_matches_query(&self, row: &DashboardRow, query: &str) -> bool {
        let mut haystacks = vec![
            row.worktree.path.to_ascii_lowercase(),
            fold_home(&row.worktree.path).to_ascii_lowercase(),
            row.worktree.branch.to_ascii_lowercase(),
            row.worktree.commit.to_ascii_lowercase(),
            // Status column text — so users can filter by the rendered label
            // (`clean`/`dirty`/`opened`/`merged`).
            status_label_and_style(row).0.to_ascii_lowercase(),
        ];
        // Ahead/Behind column text — match the rendered "+N -N" / "=0" form
        // and also raw "ahead N", "behind N" so either spelling filters.
        if let Some(branch_status) = &row.worktree.branch_status {
            if branch_status.ahead == 0 && branch_status.behind == 0 {
                haystacks.push("=0".into());
            } else {
                haystacks.push(format!(
                    "+{} -{}",
                    branch_status.ahead, branch_status.behind
                ));
                haystacks.push(format!(
                    "ahead {} behind {}",
                    branch_status.ahead, branch_status.behind
                ));
            }
        }
        if let Some(commit) = &row.last_commit {
            haystacks.push(commit.sha.to_ascii_lowercase());
            haystacks.push(commit.summary.to_ascii_lowercase());
            haystacks.push(commit.author.to_ascii_lowercase());
            haystacks.push(commit.relative_time.to_ascii_lowercase());
        }
        if let Some(pr) = &row.pull_request {
            haystacks.push(pr.title.to_ascii_lowercase());
            haystacks.push(pr.url.to_ascii_lowercase());
        }
        haystacks
            .into_iter()
            .any(|haystack| haystack.contains(query))
    }

    fn selected_row_index(&self) -> Option<usize> {
        self.filtered_indices().get(self.selected).copied()
    }

    fn selected_row(&self) -> Option<&DashboardRow> {
        self.selected_row_index()
            .and_then(|index| self.rows.get(index))
    }

    fn status_banner(&self) -> Line<'static> {
        let dirty = self
            .rows
            .iter()
            .filter(|row| !row.worktree.is_clean)
            .count();
        let open_prs = self
            .rows
            .iter()
            .filter(|row| {
                matches!(
                    row.pull_request.as_ref().map(|pr| pr.state),
                    Some(PrState::Open)
                )
            })
            .count();
        let refreshed = match self.refreshed_at {
            Some(instant) => format_refreshed_label(instant.elapsed()),
            None => "Waiting for first refresh".to_string(),
        };
        Line::from(vec![
            Span::styled(refreshed, Style::default().fg(colors::INFO)),
            Span::styled(" - ", Style::default().fg(colors::MUTED)),
            Span::raw(format!(
                "{} worktrees, {} dirty, {} {} open",
                self.rows.len(),
                dirty,
                open_prs,
                if open_prs == 1 { "PR" } else { "PRs" }
            )),
        ])
    }

    fn search_line(&self) -> Line<'_> {
        let mut spans = vec![Span::styled(
            "Search: ",
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        )];
        if self.query.is_empty() {
            spans.push(Span::styled(
                "type to filter...",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ));
        } else {
            spans.push(Span::styled(
                self.query.clone(),
                Style::default().fg(colors::EMPHASIS),
            ));
        }
        Line::from(spans)
    }

    fn render_table(&self, frame: &mut Frame, area: Rect, layout: &DashboardTableLayout) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            let message = if self.rows.is_empty() {
                "No worktrees found."
            } else {
                "No worktrees match the current filter."
            };
            frame.render_widget(
                Paragraph::new(message).style(Style::default().fg(colors::MUTED)),
                area,
            );
            return;
        }

        let (start, end) = visible_window(self.selected, filtered.len(), MAX_VISIBLE_ROWS);
        let visible = &filtered[start..end];
        let hidden_above = start;
        let hidden_below = filtered.len().saturating_sub(end);
        let headers = self.header_cells(layout);
        let widths = self.column_widths(layout);

        let mut rows: Vec<Row> = Vec::new();

        if filtered.len() > MAX_VISIBLE_ROWS {
            rows.push(self.overflow_row(
                if hidden_above > 0 {
                    format!("↑ {hidden_above} more above")
                } else {
                    "↑ top".to_string()
                },
                hidden_above > 0,
            ));
        }

        rows.extend(visible.iter().enumerate().map(|(offset, index)| {
            let row = &self.rows[*index];
            let is_selected = start + offset == self.selected;
            let style = if is_selected {
                Style::default()
                    .bg(colors::MENU_SELECTION_BG)
                    .fg(colors::MENU_SELECTION_FG)
            } else {
                Style::default()
            };
            Row::new(self.row_cells(row, layout)).style(style)
        }));

        if filtered.len() > MAX_VISIBLE_ROWS {
            rows.push(self.overflow_row(
                if hidden_below > 0 {
                    format!("↓ {hidden_below} more below")
                } else {
                    "↓ bottom".to_string()
                },
                hidden_below > 0,
            ));
        }

        let table = Table::new(rows, widths)
            .header(
                Row::new(headers).style(
                    Style::default()
                        .fg(colors::HEADER_SUBTITLE)
                        .add_modifier(Modifier::BOLD),
                ),
            )
            .column_spacing(2);
        frame.render_widget(table, area);
    }

    fn header_cells(&self, layout: &DashboardTableLayout) -> Vec<Cell<'static>> {
        let mut cells = vec![Cell::from("Worktree")];
        for column in &layout.visible_columns {
            cells.push(Cell::from(column.title(layout.compact)));
        }
        cells
    }

    fn row_cells(&self, row: &DashboardRow, layout: &DashboardTableLayout) -> Vec<Cell<'static>> {
        let mut cells = vec![Cell::from(Line::from(vec![
            Span::styled(
                truncate(
                    &fold_home(&row.worktree.path),
                    layout.worktree_width as usize,
                ),
                Style::default().fg(colors::EMPHASIS),
            ),
            Span::styled(
                if row.error.is_some() { " [!]" } else { "" },
                Style::default()
                    .fg(colors::WARNING)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))];

        for column in &layout.visible_columns {
            cells.push(column.cell(row, layout.compact));
        }

        cells
    }

    fn column_widths(&self, layout: &DashboardTableLayout) -> Vec<Constraint> {
        let mut widths = vec![Constraint::Length(layout.worktree_width)];
        for column in &layout.visible_columns {
            widths.push(Constraint::Length(column.width(layout.compact)));
        }
        widths
    }

    fn footer_lines(&self, width: u16, layout: &DashboardTableLayout) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some(notice) = &self.notice {
            lines.push(Line::from(Span::styled(
                notice.clone(),
                Style::default().fg(colors::INFO),
            )));
        } else if let Some(row) = self.selected_row() {
            if let Some(error) = &row.error {
                lines.push(Line::from(Span::styled(
                    format!("Selected row warning: {error}"),
                    Style::default().fg(colors::WARNING),
                )));
            } else if self.rows.iter().any(|candidate| candidate.error.is_some()) {
                lines.push(Line::from(Span::styled(
                    "Some worktrees have refresh warnings. Move the selection onto [!] rows to inspect them.",
                    Style::default().fg(colors::WARNING),
                )));
            } else if let Some(warning) = self.warnings.first() {
                lines.push(Line::from(Span::styled(
                    warning.clone(),
                    Style::default().fg(colors::WARNING),
                )));
            } else if let Some(detail) = self.selected_detail_line(width, row, layout) {
                lines.push(detail);
            }
        }

        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "↑↓ Navigate  ↵ Actions  Type to Search  Ctrl+R Refresh  Esc Clear / Back",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "↑↓ Navigate  ↵ Actions  Type to Search  Ctrl+R Refresh  Esc Clear / Back",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            )));
        }

        let muted_dim = Style::default().fg(colors::MUTED).add_modifier(Modifier::DIM);
        lines.push(Line::from(vec![
            Span::styled("Status: ", muted_dim),
            Span::styled("Clean", Style::default().fg(colors::ACCENT)),
            Span::styled(" = no uncommitted changes  ", muted_dim),
            Span::styled("Dirty", Style::default().fg(colors::ERROR)),
            Span::styled(" = has uncommitted changes  ", muted_dim),
            Span::styled("Opened", Style::default().fg(colors::WARNING)),
            Span::styled(" = PR open  ", muted_dim),
            Span::styled("Merged", Style::default().fg(colors::INFO)),
            Span::styled(" = PR merged", muted_dim),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Ahead/Behind: ", Style::default().fg(colors::MUTED).add_modifier(Modifier::DIM)),
            Span::styled("+N", Style::default().fg(colors::SUCCESS)),
            Span::styled(" lines added  ", Style::default().fg(colors::MUTED).add_modifier(Modifier::DIM)),
            Span::styled("-N", Style::default().fg(colors::ERROR)),
            Span::styled(" lines removed vs upstream/main (falls back to upstream/master, origin/main, origin/master)", Style::default().fg(colors::MUTED).add_modifier(Modifier::DIM)),
        ]));
        lines
    }

    fn render_action_menu(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(area);

        if let Some(index) = self.action_target.or_else(|| self.selected_row_index()) {
            let row = &self.rows[index];
            let header = Line::from(vec![
                Span::raw("Selected: "),
                Span::styled(
                    fold_home(&row.worktree.path),
                    Style::default().fg(colors::PRIMARY),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("({})", row.worktree.branch),
                    Style::default().fg(colors::SUCCESS),
                ),
            ]);
            frame.render_widget(Paragraph::new(header), chunks[0]);
        }

        if let Some(select) = &self.action_select {
            select.render(frame, chunks[1]);
        }
    }

    fn table_layout(&self, width: u16) -> DashboardTableLayout {
        let compact = width < 90;
        let spacing = 2u16;
        let min_worktree = if compact { 14 } else { 18 };
        let mut used = 0u16;
        let mut visible_columns = Vec::new();
        let mut hidden_columns = Vec::new();

        for column in &self.columns {
            let candidate_width = column.width(compact);
            let next_used = used.saturating_add(candidate_width).saturating_add(spacing);
            let remaining_for_worktree = width.saturating_sub(next_used);
            if remaining_for_worktree >= min_worktree {
                visible_columns.push(*column);
                used = next_used;
            } else {
                hidden_columns.push(*column);
                hidden_columns.extend(self.columns.iter().skip(visible_columns.len() + 1).copied());
                break;
            }
        }

        let worktree_width = width.saturating_sub(used).max(min_worktree);

        DashboardTableLayout {
            worktree_width,
            visible_columns,
            hidden_columns,
            compact,
        }
    }

    fn selected_detail_line(
        &self,
        width: u16,
        row: &DashboardRow,
        layout: &DashboardTableLayout,
    ) -> Option<Line<'static>> {
        let mut spans = Vec::new();
        let muted = Style::default().fg(colors::MUTED);
        let emphasis = Style::default().fg(colors::EMPHASIS);
        let info = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);

        if !layout.hidden_columns.is_empty() {
            spans.push(Span::styled(
                format!("Narrow view: {} hidden", layout.hidden_columns.len()),
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        if let Some(pr) = &row.pull_request {
            if !spans.is_empty() {
                spans.push(Span::styled("  •  ", muted));
            }
            spans.push(Span::styled("PR ", muted));
            spans.push(Span::styled(
                format!("#{} {}", pr.number, pr.state.label()),
                pr.state.style().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(": ", muted));
            spans.push(Span::styled(
                truncate(
                    &pr.title,
                    if width >= 100 {
                        52
                    } else if width >= 80 {
                        34
                    } else {
                        18
                    },
                ),
                emphasis,
            ));
        }

        for column in &layout.hidden_columns {
            match column {
                DashboardColumn::LastCommit => {
                    if let Some(commit) = &row.last_commit {
                        if !spans.is_empty() {
                            spans.push(Span::styled("  •  ", muted));
                        }
                        spans.push(Span::styled("Commit ", muted));
                        spans.push(Span::styled(commit.sha.clone(), info));
                        spans.push(Span::styled(" ", muted));
                        spans.push(Span::styled(
                            truncate(&commit.summary, if width >= 90 { 22 } else { 14 }),
                            emphasis,
                        ));
                    }
                }
                DashboardColumn::PullRequest => {}
                DashboardColumn::Branch
                | DashboardColumn::Status
                | DashboardColumn::AheadBehind => {}
            }
        }

        if spans.is_empty() {
            None
        } else {
            Some(Line::from(spans))
        }
    }

    fn overflow_row(&self, message: String, active: bool) -> Row<'static> {
        let mut cells = vec![Cell::from(message)];
        for _ in &self.columns {
            cells.push(Cell::from(""));
        }
        let style = if active {
            Style::default().fg(colors::ACCENT)
        } else {
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM)
        };
        Row::new(cells).style(style)
    }

    fn render_error(&self, frame: &mut Frame, area: Rect, error: &str) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Min(1),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new("Dashboard unavailable").style(
                Style::default()
                    .fg(colors::ERROR)
                    .add_modifier(Modifier::BOLD),
            ),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(error).style(Style::default().fg(colors::EMPHASIS)),
            chunks[1],
        );
        frame.render_widget(
            Paragraph::new("Press r to retry or Esc to go back").style(
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
            chunks[2],
        );
    }
}

impl DashboardColumn {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "branch" => Some(Self::Branch),
            "status" => Some(Self::Status),
            "ahead_behind" => Some(Self::AheadBehind),
            "last_commit" => Some(Self::LastCommit),
            "pull_request" => Some(Self::PullRequest),
            _ => None,
        }
    }

    fn title(self, compact: bool) -> &'static str {
        match self {
            Self::Branch => "Branch",
            Self::Status => "Status",
            Self::AheadBehind => {
                if compact {
                    "A/B"
                } else {
                    "Ahead/Behind"
                }
            }
            Self::LastCommit => {
                if compact {
                    "Commit"
                } else {
                    "Last Commit"
                }
            }
            Self::PullRequest => "PR",
        }
    }

    fn width(self, compact: bool) -> u16 {
        match self {
            Self::Branch => {
                if compact {
                    12
                } else {
                    18
                }
            }
            Self::Status => {
                if compact {
                    7
                } else {
                    10
                }
            }
            Self::AheadBehind => {
                if compact {
                    10
                } else {
                    12
                }
            }
            Self::LastCommit => {
                if compact {
                    16
                } else {
                    22
                }
            }
            Self::PullRequest => {
                if compact {
                    12
                } else {
                    18
                }
            }
        }
    }

    fn cell(self, row: &DashboardRow, compact: bool) -> Cell<'static> {
        match self {
            Self::Branch => Cell::from(Line::from(Span::raw(truncate(
                &row.worktree.branch,
                self.width(compact) as usize,
            )))),
            Self::Status => {
                let (text, style) = status_label_and_style(row);
                Cell::from(Line::from(Span::styled(text, style)))
            }
            Self::AheadBehind => match row.worktree.branch_status.as_ref() {
                Some(branch_status) if branch_status.ahead == 0 && branch_status.behind == 0 => {
                    Cell::from(Line::from(Span::styled(
                        "=0",
                        Style::default().fg(colors::MUTED),
                    )))
                }
                Some(branch_status) => Cell::from(Line::from(vec![
                    Span::styled(
                        format!("+{}", branch_status.ahead),
                        Style::default().fg(colors::SUCCESS),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("-{}", branch_status.behind),
                        Style::default().fg(colors::ERROR),
                    ),
                ])),
                None => Cell::from(Line::from(Span::styled(
                    "-",
                    Style::default().fg(colors::MUTED),
                ))),
            },
            Self::LastCommit => {
                let text = row
                    .last_commit
                    .as_ref()
                    .map(|commit| {
                        format!(
                            "{} {}",
                            commit.sha,
                            truncate(
                                &commit.summary,
                                self.width(compact).saturating_sub(9) as usize
                            )
                        )
                    })
                    .unwrap_or_else(|| "-".to_string());
                Cell::from(text)
            }
            Self::PullRequest => {
                if let Some(pr) = &row.pull_request {
                    let style = pr.state.style();
                    Cell::from(Line::from(Span::styled(
                        if compact {
                            format!("#{} {}", pr.number, pr.state.short_label())
                        } else {
                            format!("#{} {}", pr.number, pr.state.label())
                        },
                        style,
                    )))
                } else {
                    Cell::from("-")
                }
            }
        }
    }
}

impl PrState {
    fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Merged => "Merged",
            Self::Closed => "Closed",
            Self::Draft => "Draft",
        }
    }

    fn short_label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Merged => "Merged",
            Self::Closed => "Closed",
            Self::Draft => "Draft",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Open => Style::default().fg(colors::INFO),
            Self::Merged => Style::default()
                .fg(colors::SUCCESS)
                .add_modifier(Modifier::DIM),
            Self::Closed => Style::default()
                .fg(colors::ERROR)
                .add_modifier(Modifier::DIM),
            Self::Draft => Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        }
    }
}

fn status_label_and_style(row: &DashboardRow) -> (&'static str, Style) {
    match row.pull_request.as_ref().map(|pr| pr.state) {
        Some(PrState::Merged) => ("Merged", Style::default().fg(colors::INFO)),
        Some(PrState::Open) => ("Opened", Style::default().fg(colors::WARNING)),
        _ if row.worktree.is_clean => ("Clean", Style::default().fg(colors::ACCENT)),
        _ => ("Dirty", Style::default().fg(colors::ERROR)),
    }
}

fn visible_window(selected: usize, total: usize, max_visible: usize) -> (usize, usize) {
    if total <= max_visible {
        return (0, total);
    }
    let half = max_visible / 2;
    let mut start = selected.saturating_sub(half);
    let mut end = (start + max_visible).min(total);
    if end - start < max_visible {
        start = end.saturating_sub(max_visible);
    }
    end = (start + max_visible).min(total);
    (start, end)
}

fn format_elapsed(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    if secs < 2 {
        return "just now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    format!("{}h", mins / 60)
}

fn format_refreshed_label(duration: std::time::Duration) -> String {
    let elapsed = format_elapsed(duration);
    if elapsed == "just now" {
        "Refreshed just now".to_string()
    } else {
        format!("Refreshed {elapsed} ago")
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max_chars {
        return value.to_string();
    }
    chars[..max_chars.saturating_sub(1)]
        .iter()
        .collect::<String>()
        + "..."
}

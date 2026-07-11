//! Shared confirmation layout for the AI-assisted Pull Request commands
//! (Bugkill, Enrich, Fix, Update/Upload, Merge). It renders the same
//! top-to-bottom stack on every page so all five read as one component:
//!
//! ```text
//! <title>                        ← command color, bold
//!
//! PR          #12 (Open)         ← labeled detail rows (optional PR line)
//! Branch      feat/thing
//! Worktree    /tmp/repo
//!
//! Will run:                      ← numbered step preview
//!   1. …
//!   2. …
//!
//! Role         Model      Thinking   ← centered "which AIs run" table
//! enrich       glm-5.2    max
//!
//! ┌──────── Are you sure ────────┐   ← ConfirmationModal (Yes / No)
//! └──────────────────────────────┘
//! ```
//!
//! Bugkill is the reference design; the other screens feed this view their
//! own detail rows, step text, and resolved per-role model config. Every
//! section except the title is optional — an empty details / steps / AI list
//! simply drops that slot together with its trailing blank line, so a command
//! that has no AI (Merge) or wants no `Will run:` preview reuses the exact
//! same layout math.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell as TableCell, Paragraph, Row, Table};
use ratatui::Frame;

use super::confirmation_modal::ConfirmationModal;
use crate::messages::colors;

/// Rows reserved for the `ConfirmationModal` slot. Matches the footprint the
/// modal auto-centers into on the Bugkill confirm page.
const MODAL_HEIGHT: u16 = 12;

/// Fixed column width every detail label is padded to. Keeps values aligned
/// across commands. Labels must stay shorter than this so a space always
/// separates the label from its value — a label of exactly `LABEL_WIDTH`
/// chars would butt straight up against the value (which is why the Merge
/// page splits the 12-char "Ahead/Behind" into short "Ahead"/"Behind" rows).
const LABEL_WIDTH: usize = 12;

/// One `Role / Model / Thinking` row in the "which AIs will run" table. The
/// `role` label mirrors the command's `ai.*` config key (e.g. `investigate`,
/// `plan`, `apply`, `enrich`, `update`) so a reader can map the row straight
/// back to the setting that drives it.
#[derive(Debug, Clone)]
pub struct AiRoleRow {
    role: String,
    role_color: Color,
    model: String,
    thinking: String,
}

impl AiRoleRow {
    pub fn new(
        role: impl Into<String>,
        role_color: Color,
        model: impl Into<String>,
        thinking: impl Into<String>,
    ) -> Self {
        Self {
            role: role.into(),
            role_color,
            model: model.into(),
            thinking: thinking.into(),
        }
    }
}

/// The canonical detail row shared by every PR confirm panel: a `label`
/// padded to [`LABEL_WIDTH`] in muted/dim, followed by the styled `value` and
/// an optional dim `trailing` note. The fixed width keeps labels aligned
/// across commands ("Base ref", "Worktree", "Last commit", …).
pub fn labeled_line(
    label: &str,
    value: Span<'static>,
    trailing: Option<Span<'static>>,
) -> Line<'static> {
    let mut values: Vec<Span<'static>> = Vec::with_capacity(2);
    values.push(value);
    if let Some(extra) = trailing {
        values.push(extra);
    }
    labeled_spans(label, values)
}

/// Like [`labeled_line`] but for a value made of several independently styled
/// spans — e.g. a commit's sha, summary, and relative time each in their own
/// color. The `label` is padded to the same [`LABEL_WIDTH`] so these rows line
/// up with plain `labeled_line`s on the same panel.
pub fn labeled_spans(label: &str, values: Vec<Span<'static>>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(values.len() + 1);
    spans.push(Span::styled(
        format!("{label:<LABEL_WIDTH$}"),
        Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM),
    ));
    spans.extend(values);
    Line::from(spans)
}

/// A `Will run:` header followed by numbered steps, matching the Bugkill
/// pipeline preview. Callers pass the human-readable step text; the widget
/// owns the numbering + styling so every command's preview looks the same.
pub fn will_run_lines<S: AsRef<str>>(steps: &[S]) -> Vec<Line<'static>> {
    let header_style = Style::default()
        .fg(colors::INFO)
        .add_modifier(Modifier::BOLD);
    let number_style = Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM);
    let text_style = Style::default().fg(colors::EMPHASIS);
    let mut lines = vec![Line::from(Span::styled(
        "Will run:".to_string(),
        header_style,
    ))];
    for (i, text) in steps.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!("  {}. ", i + 1), number_style),
            Span::styled(text.as_ref().to_string(), text_style),
        ]));
    }
    lines
}

/// Builder for the shared PR-command confirm layout. Build it fresh each
/// render from the screen's request + resolved config, chain the sections
/// that apply, then `render` into the panel area.
pub struct PrConfirmView<'a> {
    title: String,
    /// Color for the title heading. Defaults to `BRAND`; each PR command
    /// overrides it with its signature color (Enrich purple, Merge green,
    /// …) so the confirm screen matches the command's dashboard button.
    title_color: Color,
    /// Ordered text blocks (details, `Will run:` preview, description snippet,
    /// …) rendered blank-line-separated after the title. Empty blocks are
    /// dropped so they take up no space.
    blocks: Vec<Vec<Line<'static>>>,
    ai_roles: Vec<AiRoleRow>,
    modal: Option<&'a ConfirmationModal>,
}

impl<'a> PrConfirmView<'a> {
    /// Start a view with `title` (rendered in the command color + bold, like
    /// Bugkill's "Hunt a bug on this worktree?"). Defaults to `BRAND` until a
    /// command overrides it via [`Self::title_color`].
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            title_color: colors::BRAND,
            blocks: Vec::new(),
            ai_roles: Vec::new(),
            modal: None,
        }
    }

    /// Set the title heading color to the command's signature color so the
    /// confirm screen's heading matches both the dashboard button and the
    /// accent of the confirmation modal drawn below it.
    pub fn title_color(mut self, color: Color) -> Self {
        self.title_color = color;
        self
    }

    /// Append a text block (e.g. the labeled detail rows). No-op when empty.
    pub fn block(mut self, lines: Vec<Line<'static>>) -> Self {
        if !lines.is_empty() {
            self.blocks.push(lines);
        }
        self
    }

    /// Append a `Will run:` numbered-step preview. No-op when `steps` is empty.
    pub fn steps<S: AsRef<str>>(self, steps: &[S]) -> Self {
        if steps.is_empty() {
            return self;
        }
        self.block(will_run_lines(steps))
    }

    /// Set the "which AIs will run" table rows. An empty list drops the table.
    pub fn ai_roles(mut self, roles: Vec<AiRoleRow>) -> Self {
        self.ai_roles = roles;
        self
    }

    /// Attach the confirmation modal rendered at the bottom of the panel.
    pub fn modal(mut self, modal: Option<&'a ConfirmationModal>) -> Self {
        self.modal = modal;
        self
    }

    fn ai_table_height(&self) -> u16 {
        if self.ai_roles.is_empty() {
            0
        } else {
            // header row + one row per configured role.
            1 + self.ai_roles.len() as u16
        }
    }

    /// Total rows this view needs so a parent that sizes its panel up front
    /// (e.g. the framed Merge panel) can reserve the right height.
    pub fn content_height(&self) -> u16 {
        let mut height = 1u16; // title
        for block in &self.blocks {
            height = height.saturating_add(1); // blank separator
            height = height.saturating_add(block.len() as u16);
        }
        let table = self.ai_table_height();
        if table > 0 {
            height = height.saturating_add(1 + table);
        }
        if self.modal.is_some() {
            height = height.saturating_add(1 + MODAL_HEIGHT);
        }
        height
    }

    /// Draw the full confirm panel into `area`.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let table_height = self.ai_table_height();

        let mut constraints: Vec<Constraint> = vec![Constraint::Length(1)]; // title
        for block in &self.blocks {
            constraints.push(Constraint::Length(1)); // blank
            constraints.push(Constraint::Length(block.len() as u16));
        }
        if table_height > 0 {
            constraints.push(Constraint::Length(1)); // blank
            constraints.push(Constraint::Length(table_height));
        }
        if self.modal.is_some() {
            constraints.push(Constraint::Length(1)); // blank
            constraints.push(Constraint::Length(MODAL_HEIGHT));
        }
        constraints.push(Constraint::Min(0));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                self.title.clone(),
                Style::default()
                    .fg(self.title_color)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[0],
        );

        // Each block occupies `[blank, content]`, so content lands at odd
        // offsets past the title.
        let mut idx = 1usize;
        for block in &self.blocks {
            frame.render_widget(Paragraph::new(block.clone()), chunks[idx + 1]);
            idx += 2;
        }
        if table_height > 0 {
            self.render_ai_table(frame, chunks[idx + 1]);
            idx += 2;
        }
        if let Some(modal) = self.modal {
            modal.render(frame, chunks[idx + 1]);
        }
    }

    /// The resolved per-role config table — centered, so the user sees which
    /// models (and reasoning effort) the command will spend before confirming.
    fn render_ai_table(&self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            TableCell::from("Role"),
            TableCell::from("Model"),
            TableCell::from("Thinking"),
        ])
        .style(
            Style::default()
                .fg(colors::GRAY_DARK)
                .add_modifier(Modifier::BOLD),
        );

        let rows: Vec<Row> = self
            .ai_roles
            .iter()
            .map(|role| {
                let thinking = if role.thinking.trim().is_empty() {
                    "default".to_string()
                } else {
                    role.thinking.clone()
                };
                Row::new(vec![
                    TableCell::from(role.role.clone()).style(Style::default().fg(role.role_color)),
                    TableCell::from(role.model.clone())
                        .style(Style::default().fg(colors::GRAY_LIGHT)),
                    TableCell::from(thinking).style(Style::default().fg(colors::EMPHASIS)),
                ])
            })
            .collect();

        let table_width = area.width.min(62);
        let table_area = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(area.width.saturating_sub(table_width) / 2),
                Constraint::Length(table_width),
                Constraint::Min(0),
            ])
            .split(area)[1];

        let table = Table::new(
            rows,
            [
                Constraint::Length(12),
                Constraint::Min(24),
                Constraint::Length(10),
            ],
        )
        .header(header)
        .column_spacing(2);
        frame.render_widget(table, table_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn dump(view: &PrConfirmView, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_title_details_steps_and_ai_table() {
        let details = vec![
            labeled_line(
                "Branch",
                Span::styled("feat/x".to_string(), Style::default().fg(colors::SUCCESS)),
                None,
            ),
            labeled_line(
                "Worktree",
                Span::styled("/tmp/x".to_string(), Style::default().fg(colors::EMPHASIS)),
                None,
            ),
        ];
        let view = PrConfirmView::new("Do the thing?")
            .block(details)
            .steps(&["git fetch", "git merge base"])
            .ai_roles(vec![AiRoleRow::new(
                "resolve",
                colors::INFO,
                "glm/model",
                "",
            )]);
        let out = dump(&view, 90, 24);
        assert!(out.contains("Do the thing?"), "{out}");
        assert!(out.contains("Branch"), "{out}");
        assert!(out.contains("Worktree"), "{out}");
        assert!(out.contains("Will run:"), "{out}");
        assert!(out.contains("1. git fetch"), "{out}");
        assert!(out.contains("Role"), "{out}");
        assert!(out.contains("Model"), "{out}");
        assert!(out.contains("Thinking"), "{out}");
        assert!(out.contains("resolve"), "{out}");
        assert!(out.contains("glm/model"), "{out}");
        // Empty thinking renders as "default".
        assert!(out.contains("default"), "{out}");
    }

    #[test]
    fn omits_ai_table_when_no_roles() {
        let view = PrConfirmView::new("Merge?").block(vec![labeled_line(
            "PR",
            Span::styled("#7".to_string(), Style::default().fg(colors::INFO)),
            None,
        )]);
        let out = dump(&view, 80, 20);
        assert!(out.contains("Merge?"), "{out}");
        assert!(!out.contains("Thinking"), "{out}");
    }

    #[test]
    fn content_height_accounts_for_every_section() {
        let view = PrConfirmView::new("t")
            .block(vec![Line::from("a"), Line::from("b")]) // 2 rows + 1 blank
            .steps(&["one", "two"]) // header + 2 = 3 rows + 1 blank
            .ai_roles(vec![AiRoleRow::new("r", colors::INFO, "m", "")]); // 1 blank + (1 header + 1 row)
                                                                         // title(1) + blank(1)+details(2) + blank(1)+steps(3) + blank(1)+table(2)
        assert_eq!(view.content_height(), 1 + 1 + 2 + 1 + 3 + 1 + 2);
    }
}

//! Pull request merge confirmation layout.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::confirmation_modal::ConfirmationModal;

use crate::messages::colors;

const MODAL_HEIGHT: u16 = 12;

/// Fixed column width every detail label is padded to. Keeps values aligned
/// across detail rows. Labels must stay shorter than this so a space always
/// separates the label from its value — a label of exactly `LABEL_WIDTH`
/// chars would butt straight up against the value (which is why the Merge
/// page splits the 12-char "Ahead/Behind" into short "Ahead"/"Behind" rows).
const LABEL_WIDTH: usize = 12;

/// The canonical detail row shared by every PR confirm panel: a `label`
/// padded to [`LABEL_WIDTH`] in muted/dim, followed by the styled `value` and
/// an optional dim `trailing` note. The fixed width keeps labels aligned
/// across detail rows ("Worktree", "Last commit", …).
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

/// Shared style for inline code chips: orange text on a neutral gray
/// background, distinct from the app's brown-tinted panel colors.
pub fn code_style() -> Style {
    Style::default().fg(colors::ACCENT).bg(colors::CODE_BG)
}

/// Split text by backtick-delimited code spans and return styled spans.
/// Code (text between backticks) gets `code_style`, padded with a leading and
/// trailing space so the highlight reads as a chip rather than a tight
/// underline; everything else gets `base_style`. Nesting is not supported —
/// inner backticks are treated as regular text.
pub fn code_spans(text: &str, base_style: Style, code_style: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;

    while cursor < text.len() {
        let rest = &text[cursor..];
        match rest.find('`') {
            None => {
                spans.push(Span::styled(text[cursor..].to_string(), base_style));
                break;
            }
            Some(rel_open) => {
                let open = cursor + rel_open;
                if open > cursor {
                    spans.push(Span::styled(text[cursor..open].to_string(), base_style));
                }
                let after_open = &text[open + 1..];
                match after_open.find('`') {
                    None => {
                        spans.push(Span::styled(text[open..].to_string(), base_style));
                        break;
                    }
                    Some(rel_close) => {
                        let close = open + 1 + rel_close;
                        spans.push(Span::styled(
                            format!(" {} ", &text[open + 1..close]),
                            code_style,
                        ));
                        cursor = close + 1;
                    }
                }
            }
        }
    }

    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), base_style));
    }
    spans
}

/// Backtick-aware lines for a whole block of text, one [`Line`] per `\n`
/// segment. Non-code spans are left unstyled (`Style::default()`) so callers
/// that already set a fallback style on the surrounding `Paragraph` (toasts,
/// the confirmation modal) keep their existing look; only backtick-delimited
/// segments get the code-chip treatment. Use [`code_spans`] directly when a
/// caller needs a specific base style instead of the paragraph fallback.
pub fn code_lines(text: &str) -> Vec<Line<'static>> {
    text.lines()
        .map(|line| Line::from(code_spans(line, Style::default(), code_style())))
        .collect()
}

pub fn will_run_lines<S: AsRef<str>>(steps: &[S]) -> Vec<Line<'static>> {
    let header_style = Style::default()
        .fg(colors::INFO)
        .add_modifier(Modifier::BOLD);
    let number_style = Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM);
    let text_style = Style::default().fg(colors::EMPHASIS);
    let code_style = code_style();
    let mut lines = vec![Line::from(Span::styled(
        "Will run:".to_string(),
        header_style,
    ))];
    for (i, text) in steps.iter().enumerate() {
        let mut spans = vec![Span::styled(format!("  {}. ", i + 1), number_style)];
        spans.extend(code_spans(text.as_ref(), text_style, code_style));
        lines.push(Line::from(spans));
    }
    lines
}

/// Builder for the shared PR-command confirm layout. Build it fresh each
/// render from the screen's request + resolved config, chain the sections
/// that apply, then `render` into the panel area.
pub struct PrConfirmView<'a> {
    title: String,
    title_color: Color,
    /// Ordered text blocks (details, `Will run:` preview, description snippet,
    /// …) rendered blank-line-separated after the title. Empty blocks are
    /// dropped so they take up no space.
    blocks: Vec<Vec<Line<'static>>>,

    modal: Option<&'a ConfirmationModal>,
}

impl<'a> PrConfirmView<'a> {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            title_color: colors::BRAND,
            blocks: Vec::new(),

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

    /// Attach the confirmation modal rendered at the bottom of the panel.
    pub fn modal(mut self, modal: Option<&'a ConfirmationModal>) -> Self {
        self.modal = modal;
        self
    }

    /// Total rows this view needs so a parent that sizes its panel up front
    /// (e.g. the framed Merge panel) can reserve the right height.
    pub fn content_height(&self) -> u16 {
        let mut height = 1u16; // title
        for block in &self.blocks {
            height = height.saturating_add(1); // blank separator
            height = height.saturating_add(block.len() as u16);
        }

        if self.modal.is_some() {
            height = height.saturating_add(1 + MODAL_HEIGHT);
        }
        height
    }

    /// The vertical slots this view lays `area` out into: one per section, in
    /// render order, each preceded by its blank separator.
    fn section_areas(&self, area: Rect) -> std::rc::Rc<[Rect]> {
        let mut constraints: Vec<Constraint> = vec![Constraint::Length(1)]; // title
        for block in &self.blocks {
            constraints.push(Constraint::Length(1)); // blank
            constraints.push(Constraint::Length(block.len() as u16));
        }

        if self.modal.is_some() {
            constraints.push(Constraint::Length(1)); // blank
            constraints.push(Constraint::Length(MODAL_HEIGHT));
        }
        constraints.push(Constraint::Min(0));

        Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area)
    }

    /// Draw the full confirm panel into `area`.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = self.section_areas(area);

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

        if let Some(modal) = self.modal {
            modal.render(frame, chunks[idx + 1]);
        }
    }
}

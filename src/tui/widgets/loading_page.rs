//! The one loading page every wait in the TUI shares.
//!
//! With steps, the bar fills with each finished step, the in-flight step's
//! slice shimmers, and the title estimates time left from measured steps.
//! Without steps, the bar becomes an indeterminate sweep.
//!
//! Short areas drop spacing and low-priority rows first, keeping the title,
//! bar, and active step visible.

use std::time::Duration;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::messages::{colors, loading_page as messages};
use crate::tui::widgets::spinner_frame;

const MAX_WIDTH: u16 = 100;
const INDENT: usize = 2;
const MAX_VISIBLE_STEPS: usize = 12;
const TIME_WIDTH: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Warning,
    Failed,
    Skipped,
}

impl StepStatus {
    fn is_finished(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }
}

/// One row of the page's checklist.
#[derive(Debug, Clone)]
pub struct LoadingStep {
    pub label: String,
    pub status: StepStatus,
    pub note: String,
    /// Final duration once finished, or time spent so far while running.
    pub duration: Option<Duration>,
}

impl LoadingStep {
    pub fn new(label: impl Into<String>, status: StepStatus) -> Self {
        Self {
            label: label.into(),
            status,
            note: String::new(),
            duration: None,
        }
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }

    pub fn duration(mut self, duration: Option<Duration>) -> Self {
        self.duration = duration;
        self
    }
}

struct Tally {
    count: usize,
    label: String,
    color: Color,
}

pub struct LoadingPage<'a> {
    title: String,
    accent: Color,
    tick: usize,
    subject: Option<String>,
    detail: Option<String>,
    elapsed: Option<Duration>,
    steps: &'a [LoadingStep],
    tally: Vec<Tally>,
    error: Option<String>,
    hint: Option<String>,
}

struct Plan {
    subject: bool,
    detail: bool,
    tally: bool,
    error: bool,
    hint: bool,
    gaps: bool,
    step_rows: usize,
}

impl<'a> LoadingPage<'a> {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            accent: colors::PRIMARY,
            tick: 0,
            subject: None,
            detail: None,
            elapsed: None,
            steps: &[],
            tally: Vec::new(),
            error: None,
            hint: None,
        }
    }

    pub fn tick(mut self, tick: usize) -> Self {
        self.tick = tick;
        self
    }
    pub fn accent(mut self, accent: Color) -> Self {
        self.accent = accent;
        self
    }
    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
    pub fn elapsed(mut self, elapsed: Duration) -> Self {
        self.elapsed = Some(elapsed);
        self
    }
    pub fn steps(mut self, steps: &'a [LoadingStep]) -> Self {
        self.steps = steps;
        self
    }
    pub fn tally(mut self, count: usize, label: impl Into<String>, color: Color) -> Self {
        self.tally.push(Tally {
            count,
            label: label.into(),
            color,
        });
        self
    }
    pub fn error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn content_height(&self) -> u16 {
        let footer = usize::from(self.error.is_some()) + usize::from(self.hint.is_some());
        let rows = 3
            + usize::from(self.subject.is_some())
            + usize::from(self.detail.is_some())
            + match self.steps.len().min(MAX_VISIBLE_STEPS) {
                0 => 0,
                steps => 1 + steps,
            }
            + if self.tally.is_empty() { 0 } else { 2 }
            + if footer == 0 { 0 } else { 1 + footer };
        rows.min(usize::from(u16::MAX)) as u16
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let area = Rect {
            width: area.width.min(MAX_WIDTH),
            ..area
        };
        let width = usize::from(area.width);
        let body = width.saturating_sub(INDENT);
        let plan = self.plan(usize::from(area.height));
        let mut lines = vec![self.title_line(width)];
        if plan.subject {
            lines.push(indented(vec![Span::styled(
                fit(self.subject.as_deref().unwrap_or_default(), body),
                Style::default().fg(colors::WHITE),
            )]));
        }
        if plan.gaps {
            lines.push(Line::default());
        }
        if area.height > 1 {
            lines.push(indented(self.bar_spans(body)));
        }
        if plan.detail {
            lines.push(indented(vec![Span::styled(
                fit(self.detail.as_deref().unwrap_or_default(), body),
                Style::default().fg(colors::MUTED),
            )]));
        }
        if plan.step_rows > 0 {
            if plan.gaps {
                lines.push(Line::default());
            }
            lines.extend(self.step_lines(plan.step_rows, body));
        }
        if plan.tally {
            if plan.gaps {
                lines.push(Line::default());
            }
            lines.push(self.tally_line());
        }
        if plan.error || plan.hint {
            if plan.gaps {
                lines.push(Line::default());
            }
            if plan.error {
                lines.push(indented(vec![Span::styled(
                    fit(self.error.as_deref().unwrap_or_default(), body),
                    Style::default().fg(colors::ERROR),
                )]));
            }
            if plan.hint {
                lines.push(indented(vec![Span::styled(
                    fit(self.hint.as_deref().unwrap_or_default(), body),
                    Style::default().fg(colors::MUTED),
                )]));
            }
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Allocate rows by importance before drawing, leaving spacing until last.
    fn plan(&self, height: usize) -> Plan {
        fn take(budget: &mut usize, wanted: bool, rows: usize) -> bool {
            let fits = wanted && rows <= *budget;
            if fits {
                *budget -= rows;
            }
            fits
        }
        let budget = &mut height.saturating_sub(2);
        let error = take(budget, self.error.is_some(), 1);
        let first_step = take(budget, !self.steps.is_empty(), 1);
        let tally = take(budget, !self.tally.is_empty(), 1);
        let detail = take(budget, self.detail.is_some(), 1);
        let subject = take(budget, self.subject.is_some(), 1);
        let hint = take(budget, self.hint.is_some(), 1);
        let more_steps = if first_step { self.steps.len() - 1 } else { 0 }.min(*budget);
        *budget -= more_steps;
        let step_rows = usize::from(first_step) + more_steps;
        let gap_rows =
            1 + usize::from(step_rows > 0) + usize::from(tally) + usize::from(error || hint);
        let gaps = take(budget, true, gap_rows);
        Plan {
            subject,
            detail,
            tally,
            error,
            hint,
            gaps,
            step_rows,
        }
    }

    fn finished(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| step.status.is_finished())
            .count()
    }
    fn running(&self) -> Option<&LoadingStep> {
        self.steps
            .iter()
            .find(|step| step.status == StepStatus::Running)
    }
    fn estimate(&self) -> Option<Duration> {
        let measured: Vec<Duration> = self
            .steps
            .iter()
            .filter(|step| step.status.is_finished())
            .filter_map(|step| step.duration)
            .collect();
        let remaining = self.steps.len() - self.finished();
        if measured.is_empty() || remaining == 0 {
            return None;
        }
        let average = measured.iter().sum::<Duration>() / measured.len() as u32;
        let in_flight = self
            .running()
            .and_then(|step| step.duration)
            .unwrap_or_default()
            .min(average);
        Some((average * remaining as u32).saturating_sub(in_flight))
    }

    fn title_line(&self, width: usize) -> Line<'static> {
        let all_finished = !self.steps.is_empty() && self.finished() == self.steps.len();
        let (icon, icon_color) = if self.error.is_some() {
            ("✗", colors::ERROR)
        } else if all_finished {
            ("✓", colors::SUCCESS)
        } else {
            (spinner_frame(self.tick), self.accent)
        };
        let clock = [
            self.elapsed.map(clock),
            self.estimate()
                .map(|left| messages::time_left(&short_duration(left))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let room = width.saturating_sub(INDENT);
        let clock = if text_width(&clock) + 2 < room {
            clock
        } else {
            String::new()
        };
        let clock_width = text_width(&clock);
        let title = fit(&self.title, room.saturating_sub(clock_width + 2));
        let gap = room.saturating_sub(text_width(&title) + clock_width);
        Line::from(vec![
            Span::styled(icon.to_string(), Style::default().fg(icon_color)),
            Span::raw(" "),
            Span::styled(
                title,
                Style::default()
                    .fg(self.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(gap)),
            Span::styled(clock, Style::default().fg(colors::MUTED)),
        ])
    }

    fn bar_spans(&self, width: usize) -> Vec<Span<'static>> {
        if self.steps.is_empty() {
            return sweep_bar(width, self.tick, self.accent);
        }
        let total = self.steps.len();
        let done = self.finished();
        let digits = total.to_string().len();
        let count = format!("   {done:>digits$}/{total}");
        let percent = format!("  {:>3}%", done * 100 / total);
        let bar_width = width.saturating_sub(text_width(&count) + text_width(&percent));
        let shimmer = self.running().map(|_| self.tick);
        let mut spans = progress_bar(bar_width, done, total, shimmer, self.accent);
        spans.push(Span::styled(count, Style::default().fg(colors::EMPHASIS)));
        spans.push(Span::styled(
            percent,
            Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::BOLD),
        ));
        spans
    }

    fn step_lines(&self, rows: usize, width: usize) -> Vec<Line<'static>> {
        let total = self.steps.len();
        let active = [StepStatus::Running, StepStatus::Failed, StepStatus::Pending]
            .into_iter()
            .find_map(|status| self.steps.iter().position(|step| step.status == status))
            .unwrap_or(total.saturating_sub(1));
        let (start, end) = window(total, active, rows);
        let indicators = total > rows && rows >= 3;
        let timed = self.steps.iter().any(|step| step.duration.is_some());
        let muted = Style::default().fg(colors::MUTED);
        let mut lines = Vec::with_capacity(rows);
        if indicators && start > 0 {
            lines.push(indented(vec![Span::styled(
                messages::more(start, true),
                muted,
            )]));
        }
        lines.extend(
            self.steps[start..end]
                .iter()
                .map(|step| self.step_line(step, width, timed)),
        );
        if indicators && end < total {
            lines.push(indented(vec![Span::styled(
                messages::more(total - end, false),
                muted,
            )]));
        }
        lines
    }

    fn step_line(&self, step: &LoadingStep, width: usize, timed: bool) -> Line<'static> {
        let (icon, icon_color) = match step.status {
            StepStatus::Pending => ("○", colors::MUTED),
            StepStatus::Running => (spinner_frame(self.tick), self.accent),
            StepStatus::Done => ("✓", colors::SUCCESS),
            StepStatus::Warning => ("!", colors::WARNING),
            StepStatus::Failed => ("✗", colors::ERROR),
            StepStatus::Skipped => ("–", colors::MUTED),
        };
        let label_style = match step.status {
            StepStatus::Pending | StepStatus::Skipped => Style::default().fg(colors::MUTED),
            StepStatus::Running => Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD),
            _ => Style::default().fg(colors::EMPHASIS),
        };
        let note_color = match step.status {
            StepStatus::Running => self.accent,
            StepStatus::Warning => colors::WARNING,
            StepStatus::Failed => colors::ERROR,
            _ => colors::MUTED,
        };
        let row = width.saturating_sub(2);
        let time_width = if timed { TIME_WIDTH + 2 } else { 0 };
        let label_width = text_width(&step.label);
        let note_room = row
            .saturating_sub(time_width)
            .saturating_sub(label_width.min(row / 2) + 2);
        let note = fit(&step.note, note_room);
        let right = time_width
            + if note.is_empty() {
                0
            } else {
                text_width(&note) + 2
            };
        let label = fit(&step.label, row.saturating_sub(right));
        let gap = row.saturating_sub(text_width(&label) + right);
        let mut spans = vec![
            Span::raw(" ".repeat(INDENT)),
            Span::styled(icon.to_string(), Style::default().fg(icon_color)),
            Span::raw(" "),
            Span::styled(label, label_style),
            Span::raw(" ".repeat(gap)),
        ];
        if !note.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(note, Style::default().fg(note_color)));
        }
        if timed {
            let time = step.duration.map(short_duration).unwrap_or_default();
            spans.push(Span::styled(
                format!("  {time:>TIME_WIDTH$}"),
                Style::default().fg(colors::MUTED),
            ));
        }
        Line::from(spans)
    }

    fn tally_line(&self) -> Line<'static> {
        let mut spans = Vec::new();
        for (i, tally) in self.tally.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ·  ", Style::default().fg(colors::MUTED)));
            }
            let count_style = if tally.count == 0 {
                Style::default().fg(colors::MUTED)
            } else {
                Style::default()
                    .fg(tally.color)
                    .add_modifier(Modifier::BOLD)
            };
            spans.push(Span::styled(tally.count.to_string(), count_style));
            spans.push(Span::styled(
                format!(" {}", tally.label),
                Style::default().fg(colors::EMPHASIS),
            ));
        }
        indented(spans)
    }
}

/// A determinate bar at half-cell resolution, with a shimmering in-flight slice.
pub fn progress_bar(
    width: usize,
    done: usize,
    total: usize,
    shimmer: Option<usize>,
    accent: Color,
) -> Vec<Span<'static>> {
    let total = total.max(1);
    let done = done.min(total);
    let halves = width * 2 * done / total;
    let (full, half) = (halves / 2, halves % 2 == 1);
    let flight_start = full + usize::from(half);
    let flight_end = match shimmer {
        Some(_) if done < total => (width * (done + 1)).div_ceil(total).min(width),
        _ => 0,
    };
    let cells = (0..width).map(|i| {
        if i < full {
            ('━', accent)
        } else if i == full && half {
            ('╸', accent)
        } else {
            let glyph = if i == full && full > 0 { '╺' } else { '━' };
            let color = match shimmer {
                Some(tick) if i < flight_end => {
                    glint(i - flight_start, flight_end - flight_start, tick, accent)
                }
                _ => colors::PROGRESS_TRACK,
            };
            (glyph, color)
        }
    });
    spans_from_cells(cells)
}

/// An indeterminate bar: a comet with a fading tail sweeps left to right.
pub fn sweep_bar(width: usize, tick: usize, accent: Color) -> Vec<Span<'static>> {
    let tail = (width / 5).clamp(4, 14);
    let head = (tick * 2) % (width + tail);
    let cells = (0..width).map(|i| {
        let color = match head.checked_sub(i) {
            Some(distance) if distance < tail => blend(
                colors::PROGRESS_TRACK,
                accent,
                1.0 - distance as f32 / tail as f32,
            ),
            _ => colors::PROGRESS_TRACK,
        };
        ('━', color)
    });
    spans_from_cells(cells)
}

fn glint(offset: usize, len: usize, tick: usize, accent: Color) -> Color {
    const TAIL: usize = 4;
    let head = tick % (len + TAIL);
    let glow = match head.checked_sub(offset) {
        Some(distance) if distance < TAIL => 1.0 - distance as f32 / TAIL as f32,
        _ => 0.0,
    };
    blend(colors::PROGRESS_TRACK, accent, 0.3 + 0.6 * glow)
}

fn spans_from_cells(cells: impl Iterator<Item = (char, Color)>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_color = None;
    for (glyph, color) in cells {
        if run_color != Some(color) {
            if let Some(previous) = run_color {
                spans.push(Span::styled(
                    std::mem::take(&mut run),
                    Style::default().fg(previous),
                ));
            }
            run_color = Some(color);
        }
        run.push(glyph);
    }
    if let Some(color) = run_color {
        spans.push(Span::styled(run, Style::default().fg(color)));
    }
    spans
}

fn blend(from: Color, to: Color, amount: f32) -> Color {
    let amount = amount.clamp(0.0, 1.0);
    match (from, to) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let mix = |a: u8, b: u8| {
                (f32::from(a) + (f32::from(b) - f32::from(a)) * amount).round() as u8
            };
            Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
        }
        _ if amount < 0.5 => from,
        _ => to,
    }
}

/// Keep the active step visible, reserving clipped ends for "N more" indicators.
fn window(total: usize, active: usize, rows: usize) -> (usize, usize) {
    if total <= rows {
        return (0, total);
    }
    if rows < 3 {
        let start = active.min(total - rows);
        return (start, start + rows);
    }
    let visible = rows - 2;
    let start = active.saturating_sub(visible / 2).min(total - visible);
    if start == 0 {
        (0, visible + 1)
    } else if start + visible == total {
        (start - 1, total)
    } else {
        (start, start + visible)
    }
}

fn indented(mut spans: Vec<Span<'static>>) -> Line<'static> {
    spans.insert(0, Span::raw(" ".repeat(INDENT)));
    Line::from(spans)
}
fn text_width(text: &str) -> usize {
    Span::raw(text).width()
}
fn fit(text: &str, max: usize) -> String {
    if text_width(text) <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    let mut buffer = [0u8; 4];
    for c in text.chars() {
        let width = text_width(c.encode_utf8(&mut buffer));
        if used + width + 1 > max {
            break;
        }
        out.push(c);
        used += width;
    }
    if max > 0 {
        out.push('…');
    }
    out
}
fn clock(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}
fn short_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 10 {
        format!("{:.1}s", duration.as_secs_f32())
    } else if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, seconds / 60 % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw(page: &LoadingPage, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| page.render(f, f.area())).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_batch_shows_progress_timings_and_an_estimate_for_the_rest() {
        let steps = vec![
            LoadingStep::new("feature/login", StepStatus::Done)
                .note("deleted")
                .duration(Some(Duration::from_secs(12))),
            LoadingStep::new("feature/payments", StepStatus::Running)
                .note("deleting")
                .duration(Some(Duration::from_secs(4))),
            LoadingStep::new("feature/search", StepStatus::Pending),
            LoadingStep::new("feature/export", StepStatus::Pending),
        ];
        let page = LoadingPage::new("Deleting 4 worktrees")
            .elapsed(Duration::from_secs(42))
            .steps(&steps)
            .tally(1, "deleted", colors::SUCCESS)
            .tally(0, "failed", colors::ERROR);
        let lines = draw(&page, 90, page.content_height());
        assert_eq!(usize::from(page.content_height()), lines.len());
        assert!(lines[0].ends_with("0:42 · ~32s left"), "{lines:#?}");
        assert!(lines[2].ends_with("1/4   25%"), "{lines:#?}");
        assert!(lines[4].contains("✓ feature/login"), "{lines:#?}");
        assert!(lines[4].ends_with("deleted      12s"), "{lines:#?}");
        assert!(lines[5].ends_with("deleting     4.0s"), "{lines:#?}");
        assert!(lines[6].contains("○ feature/search"), "{lines:#?}");
        assert_eq!(lines[9], "  1 deleted  ·  0 failed");
    }

    #[test]
    fn a_single_operation_sweeps_instead_of_claiming_a_percentage() {
        let page = LoadingPage::new("Creating worktree...")
            .subject("feature/payments")
            .detail("Copying files");
        let lines = draw(&page, 60, page.content_height());
        assert_eq!(page.content_height(), 5);
        assert!(lines[0].starts_with("⠋ Creating worktree..."));
        assert_eq!(lines[1], "  feature/payments");
        assert!(lines[3].starts_with("  ━━━━"));
        assert!(!lines[3].contains('%'));
        assert_eq!(lines[4], "  Copying files");
    }

    #[test]
    fn a_short_area_drops_spacing_and_keeps_the_active_step_in_view() {
        let steps: Vec<LoadingStep> = (0..20)
            .map(|i| {
                let status = match i {
                    0..=9 => StepStatus::Done,
                    10 => StepStatus::Running,
                    _ => StepStatus::Pending,
                };
                LoadingStep::new(format!("worktree-{i}"), status)
            })
            .collect();
        let page = LoadingPage::new("Deleting 20 worktrees")
            .steps(&steps)
            .tally(10, "deleted", colors::SUCCESS);
        let lines = draw(&page, 60, 8);
        assert!(lines[0].contains("Deleting 20 worktrees"));
        assert!(lines[1].contains("10/20   50%"), "{lines:#?}");
        assert!(lines[2].contains('↑'), "{lines:#?}");
        assert!(
            lines.iter().any(|line| line.contains("worktree-10")),
            "{lines:#?}"
        );
        assert!(lines[6].contains('↓'), "{lines:#?}");
        assert_eq!(lines[7], "  10 deleted");
    }

    #[test]
    fn a_failure_stops_the_spinner_and_keeps_the_error_and_hint() {
        let steps = vec![
            LoadingStep::new("one", StepStatus::Failed).note("locked"),
            LoadingStep::new("two", StepStatus::Skipped),
        ];
        let page = LoadingPage::new("Deleting 2 worktrees")
            .steps(&steps)
            .error("worktree is locked")
            .hint("Press any key to try again...");
        let lines = draw(&page, 60, page.content_height());
        assert!(lines[0].starts_with("✗ Deleting 2 worktrees"));
        assert!(lines[4].contains("✗ one") && lines[4].ends_with("locked"));
        assert_eq!(lines[7], "  worktree is locked");
        assert_eq!(lines[8], "  Press any key to try again...");
    }

    #[test]
    fn long_labels_are_cut_with_an_ellipsis_and_keep_the_note() {
        let steps = vec![LoadingStep::new("x".repeat(80), StepStatus::Running).note("deleting")];
        let page = LoadingPage::new("Deleting").steps(&steps);
        let lines = draw(&page, 40, page.content_height());
        assert!(lines[4].contains('…'), "{lines:#?}");
        assert!(lines[4].ends_with("deleting"), "{lines:#?}");
        assert!(lines.iter().all(|line| text_width(line) <= 40));
    }

    #[test]
    fn the_bar_fills_at_half_cell_resolution() {
        let text = |spans: Vec<Span>| {
            spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert_eq!(
            text(progress_bar(10, 0, 4, None, colors::PRIMARY)),
            "━".repeat(10)
        );
        assert_eq!(
            text(progress_bar(10, 1, 4, None, colors::PRIMARY)),
            "━━╸━━━━━━━"
        );
        assert_eq!(
            text(progress_bar(10, 1, 2, None, colors::PRIMARY)),
            "━━━━━╺━━━━"
        );
        assert_eq!(progress_bar(10, 2, 2, Some(3), colors::PRIMARY).len(), 1);
    }

    #[test]
    fn the_sweep_moves_with_the_tick() {
        let lit = |tick| -> usize {
            sweep_bar(30, tick, colors::PRIMARY)
                .iter()
                .take_while(|span| span.style.fg == Some(colors::PROGRESS_TRACK))
                .map(|span| span.content.chars().count())
                .sum()
        };
        assert_eq!(lit(3), 1);
        assert_eq!(lit(6), 7);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(short_duration(Duration::from_millis(4200)), "4.2s");
        assert_eq!(short_duration(Duration::from_secs(42)), "42s");
        assert_eq!(short_duration(Duration::from_secs(185)), "3m 05s");
        assert_eq!(short_duration(Duration::from_secs(3720)), "1h 02m");
        assert_eq!(clock(Duration::from_secs(65)), "1:05");
        assert_eq!(clock(Duration::from_secs(3725)), "1:02:05");
    }
}

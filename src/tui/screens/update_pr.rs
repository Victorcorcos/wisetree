//! Update Pull Request confirmation screen. Three-step state machine:
//!
//! - `Loading`  : spinner while `App` resolves the base ref against the
//!   priority list (`upstream/main → upstream/master → origin/main →
//!   origin/master`).
//! - `Confirm`  : details panel on top, `ConfirmDialog` (Yes/No, **No**
//!   default) on the bottom. Enter on Yes returns `UpdateAction::Confirmed`.
//! - `Updating` : spinner with a phase-specific label on top, plus a
//!   bordered "AI Activity" panel that streams the opencode subprocess's
//!   stdout/stderr lines as they arrive (auto-scrolled to the latest).
//!   Once opencode exits the panel grows a `[ Complete ] [ Cancel ]`
//!   button row at the bottom; **Complete** commits + pushes the AI
//!   resolution, **Cancel** aborts the merge.
//!
//! Async work is owned by `App`; this screen is purely a presentation
//! state machine.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::messages::colors;
use crate::services::dashboard::{AiActivityEvent, AiActivitySeverity, AiToolResultStatus};
use crate::tui::screens::dashboard::UpdatePullRequestRequest;
use crate::tui::widgets::{
    ConfirmChoice, ConfirmDialog, ConfirmOutcome, ConfirmVariant, Status, StatusIndicator,
};

const UPDATE_LOADING_MESSAGE: &str = "Resolving base ref...";
const UPDATE_RUNNING_MESSAGE: &str = "Updating pull request...";

/// Hard cap on the number of AI activity lines retained in memory. A
/// long opencode run can emit thousands of rows (tool calls, file edits,
/// progress dots); we only ever render the bottom slice that fits the
/// activity panel, so anything older is pure memory pressure.
const AI_LOG_MAX_LINES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStep {
    Loading,
    Confirm,
    Updating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiButton {
    Complete,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateAction {
    Continue,
    Cancelled,
    Confirmed,
    /// User pressed `Complete` after the AI finished — commit + push.
    AiComplete,
    /// User pressed `Cancel` after the AI finished — abort merge.
    AiCancel,
}

pub struct UpdatePullRequestScreen {
    request: UpdatePullRequestRequest,
    confirm: Option<ConfirmDialog>,
    /// Scroll offset from the bottom of the AI activity log. `0` means
    /// "follow the latest output". When the user wheels upward we increase
    /// this offset and preserve it as new lines arrive so the viewport stays
    /// stable instead of snapping back to the tail.
    ai_scroll: u16,
    /// Label shown next to the spinner during `Updating`. Updated as the
    /// pipeline emits `UpdatePhase` events so the user knows whether
    /// we're fetching, merging, waiting on the AI, or committing.
    phase_message: String,
    /// Streaming log of the AI subprocess output. Capped at
    /// `AI_LOG_MAX_LINES`; the activity panel always renders the bottom
    /// slice so the latest line stays visible.
    ai_log: Vec<AiActivityEvent>,
    /// `true` once the pipeline has reached `ConflictsDetected` and the
    /// AI is about to (or already started) work on the merge. Drives the
    /// AI Activity panel: when `false`, the `Updating` step renders just
    /// the spinner so the panel doesn't appear during clean merges.
    ai_active: bool,
    /// `true` once opencode has exited and the user is being asked to
    /// decide on Complete or Cancel.
    ai_done: bool,
    /// Currently focused button in the Complete/Cancel pair.
    ai_button: AiButton,
    error: Option<String>,
    step: UpdateStep,
    pub tick: usize,
}

impl UpdatePullRequestScreen {
    pub fn new(request: UpdatePullRequestRequest) -> Self {
        // If the caller already resolved the base ref (rare — usually the
        // app kicks off resolution after mounting), jump straight to
        // Confirm. Otherwise show a loading spinner until `set_base_ref`
        // fires.
        let (confirm, step) = if request.base_ref.is_some() {
            (Some(build_confirm(&request)), UpdateStep::Confirm)
        } else {
            (None, UpdateStep::Loading)
        };
        Self {
            request,
            confirm,
            ai_scroll: 0,
            phase_message: UPDATE_RUNNING_MESSAGE.to_string(),
            ai_log: Vec::new(),
            ai_active: false,
            ai_done: false,
            ai_button: AiButton::Complete,
            error: None,
            step,
            tick: 0,
        }
    }

    pub fn request(&self) -> &UpdatePullRequestRequest {
        &self.request
    }

    pub fn step(&self) -> UpdateStep {
        self.step
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Called by `App` once the background resolver has picked a
    /// reachable base ref. Builds the confirm dialog and transitions to
    /// the `Confirm` step.
    pub fn set_base_ref(&mut self, base_ref: String) {
        self.request.base_ref = Some(base_ref);
        self.confirm = Some(build_confirm(&self.request));
        self.error = None;
        self.step = UpdateStep::Confirm;
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.step = UpdateStep::Confirm;
        self.confirm = None;
    }

    pub fn start_updating(&mut self) {
        self.step = UpdateStep::Updating;
        self.phase_message = UPDATE_RUNNING_MESSAGE.to_string();
        self.ai_log.clear();
        self.ai_scroll = 0;
        self.ai_active = false;
        self.ai_done = false;
        self.ai_button = AiButton::Complete;
    }

    /// Flip on the AI Activity panel. Called by the App once the pipeline
    /// has surfaced the "handing off to AI" toast (i.e. `ConflictsDetected`),
    /// so the panel only appears for runs that actually need the AI.
    pub fn mark_ai_active(&mut self) {
        self.ai_active = true;
    }

    /// Called by the App once opencode has exited. Surfaces the
    /// Complete / Cancel button row so the user can commit or abort.
    pub fn mark_ai_done(&mut self) {
        self.ai_done = true;
        self.ai_button = AiButton::Complete;
    }

    #[cfg(test)]
    pub(crate) fn ai_active(&self) -> bool {
        self.ai_active
    }

    #[cfg(test)]
    pub(crate) fn ai_done(&self) -> bool {
        self.ai_done
    }

    #[cfg(test)]
    pub(crate) fn ai_button(&self) -> AiButton {
        self.ai_button
    }

    pub fn is_updating(&self) -> bool {
        matches!(self.step, UpdateStep::Updating)
    }

    /// Update the spinner label during `Updating` so the user knows what
    /// the pipeline is actively doing (fetching, merging, AI resolving,
    /// committing, …). No-op outside the `Updating` step.
    pub fn set_phase_message(&mut self, message: impl Into<String>) {
        let message = message.into();
        if message.is_empty() {
            return;
        }
        self.phase_message = message;
    }

    /// Append a streamed AI activity event to the log. Consecutive assistant /
    /// thinking deltas are coalesced so the panel reads as a sentence instead
    /// of a pile of token-sized fragments.
    pub fn append_ai_line(&mut self, line: impl Into<AiActivityEvent>) {
        let line = line.into();
        if line.plain_text().is_empty() {
            return;
        }
        match (self.ai_log.last_mut(), &line) {
            (
                Some(AiActivityEvent::AssistantText { content: existing }),
                AiActivityEvent::AssistantText { content },
            ) => {
                existing.push_str(content);
                return;
            }
            (
                Some(AiActivityEvent::Thinking { content: existing }),
                AiActivityEvent::Thinking { content },
            ) => {
                existing.push_str(content);
                return;
            }
            _ => {}
        }
        if self.ai_scroll > 0 {
            self.ai_scroll = self.ai_scroll.saturating_add(1);
        }
        self.ai_log.push(line);
        // Trim from the front so the latest output is always retained.
        if self.ai_log.len() > AI_LOG_MAX_LINES {
            let drop = self.ai_log.len() - AI_LOG_MAX_LINES;
            self.ai_log.drain(0..drop);
            self.ai_scroll = self.ai_scroll.saturating_sub(drop as u16);
        }
    }

    #[cfg(test)]
    pub(crate) fn ai_log_lines(&self) -> Vec<String> {
        self.ai_log
            .iter()
            .map(AiActivityEvent::plain_text)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn phase_message(&self) -> &str {
        &self.phase_message
    }

    /// Scroll the AI Activity panel up by `lines` (only meaningful while
    /// the AI is actively streaming or has just finished).
    pub fn handle_mouse_scroll_up(&mut self, lines: u16) -> bool {
        if matches!(self.step, UpdateStep::Updating) && self.ai_active {
            self.ai_scroll = self.ai_scroll.saturating_add(lines);
            return true;
        }
        false
    }

    /// Scroll the AI Activity panel down by `lines`. The render path
    /// clamps against the content height every frame, so over-scrolling is
    /// safe here.
    pub fn handle_mouse_scroll_down(&mut self, lines: u16) -> bool {
        if matches!(self.step, UpdateStep::Updating) && self.ai_active {
            self.ai_scroll = self.ai_scroll.saturating_sub(lines);
            return true;
        }
        false
    }

    #[cfg(test)]
    pub(crate) fn ai_scroll(&self) -> u16 {
        self.ai_scroll
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> UpdateAction {
        if self.error.is_some() {
            return UpdateAction::Cancelled;
        }
        if matches!(self.step, UpdateStep::Loading) {
            return match key.code {
                KeyCode::Esc => UpdateAction::Cancelled,
                _ => UpdateAction::Continue,
            };
        }
        if matches!(self.step, UpdateStep::Updating) {
            if !self.ai_done {
                // Pipeline still running; swallow keys.
                return UpdateAction::Continue;
            }
            return match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    self.ai_button = match self.ai_button {
                        AiButton::Complete => AiButton::Cancel,
                        AiButton::Cancel => AiButton::Complete,
                    };
                    UpdateAction::Continue
                }
                KeyCode::Enter => match self.ai_button {
                    AiButton::Complete => UpdateAction::AiComplete,
                    AiButton::Cancel => UpdateAction::AiCancel,
                },
                KeyCode::Char('c') | KeyCode::Char('C') => UpdateAction::AiComplete,
                KeyCode::Char('x') | KeyCode::Char('X') | KeyCode::Esc => UpdateAction::AiCancel,
                _ => UpdateAction::Continue,
            };
        }
        let dialog = match self.confirm.as_mut() {
            Some(d) => d,
            None => return UpdateAction::Cancelled,
        };
        match dialog.handle_key(key) {
            ConfirmOutcome::Confirmed => UpdateAction::Confirmed,
            ConfirmOutcome::Declined | ConfirmOutcome::Cancelled => UpdateAction::Cancelled,
            ConfirmOutcome::Pending => UpdateAction::Continue,
        }
    }

    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            UpdateStep::Loading => 3,
            UpdateStep::Updating => {
                // Pre-conflict phases (fetching, merging, pushing-clean)
                // don't need the AI Activity panel — keep the panel tall
                // only once we've flipped into AI mode so the streaming
                // output has room to breathe.
                if self.ai_active {
                    if self.ai_done {
                        28
                    } else {
                        24
                    }
                } else {
                    3
                }
            }
            UpdateStep::Confirm => {
                let detail_rows = self.detail_line_count() as u16;
                let steps_rows = self.steps_line_count() as u16;
                detail_rows
                    .saturating_add(steps_rows)
                    .saturating_add(14)
                    .max(16)
            }
        }
    }

    fn detail_line_count(&self) -> usize {
        let mut rows = 0;
        // PR / Title / URL / Branch / Worktree / Base ref / Behind
        rows += 7;
        if self.request.ahead > 0 {
            rows += 1;
        }
        rows
    }

    fn steps_line_count(&self) -> usize {
        // header + 4 bullets
        5
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if let Some(err) = self.error.as_deref() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(2), Constraint::Length(1)])
                .split(area);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("Cannot update pull request: {err}"),
                    Style::default().fg(colors::ERROR),
                ))),
                chunks[0],
            );
            frame.render_widget(
                Paragraph::new("Press any key to return to dashboard...").style(
                    Style::default()
                        .fg(colors::MUTED)
                        .add_modifier(Modifier::DIM),
                ),
                chunks[1],
            );
            return;
        }
        match self.step {
            UpdateStep::Loading => {
                StatusIndicator::new(Status::Loading, UPDATE_LOADING_MESSAGE)
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            UpdateStep::Updating => self.render_updating(frame, area),
            UpdateStep::Confirm => self.render_confirm(frame, area),
        }
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        let title_line = Line::from(Span::styled(
            format!("Update Pull Request #{}?", self.request.number),
            Style::default()
                .fg(colors::BRAND)
                .add_modifier(Modifier::BOLD),
        ));
        let detail_lines = build_detail_lines(&self.request);
        let steps_lines = build_steps_lines(self.request.base_ref.as_deref().unwrap_or("?"));

        let confirm_height: u16 = 8;
        let detail_height = detail_lines.len() as u16;
        let steps_height = steps_lines.len() as u16;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),              // title
                Constraint::Length(1),              // blank
                Constraint::Length(detail_height),  // labeled rows
                Constraint::Length(1),              // blank
                Constraint::Length(steps_height),   // steps preview
                Constraint::Length(1),              // blank
                Constraint::Length(confirm_height), // ConfirmDialog
                Constraint::Min(0),
            ])
            .split(area);

        frame.render_widget(Paragraph::new(title_line), chunks[0]);
        frame.render_widget(Paragraph::new(detail_lines), chunks[2]);
        frame.render_widget(Paragraph::new(steps_lines), chunks[4]);
        if let Some(dialog) = self.confirm.as_ref() {
            dialog.render(frame, chunks[6]);
        }
    }
}

fn build_confirm(request: &UpdatePullRequestRequest) -> ConfirmDialog {
    let base = request.base_ref.as_deref().unwrap_or("base");
    let prompt = format!(
        "Merge `{base}` into branch `{}` and push the update?",
        request.branch
    );
    ConfirmDialog::new(format!("Update Pull Request #{}", request.number), prompt)
        .with_labels("Yes", "No")
        .with_variant(ConfirmVariant::Default)
        .with_default(ConfirmChoice::Cancel)
}

impl UpdatePullRequestScreen {
    fn render_updating(&self, frame: &mut Frame, area: Rect) {
        // Pre-conflict (or "no conflict at all") runs render as just a
        // spinner — the AI Activity panel is reserved for the post-
        // `ConflictsDetected` portion of the pipeline. We also fall back
        // to the spinner-only layout when the area is too short to split.
        if !self.ai_active || area.height < 5 {
            StatusIndicator::new(Status::Loading, self.phase_message.clone())
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }

        let mut constraints = vec![
            Constraint::Length(1), // spinner line
            Constraint::Length(1), // blank
            Constraint::Min(3),    // AI Activity panel
        ];
        if self.ai_done {
            constraints.push(Constraint::Length(1)); // blank
            constraints.push(Constraint::Length(3)); // button row
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        StatusIndicator::new(Status::Loading, self.phase_message.clone())
            .with_tick(self.tick)
            .render(frame, chunks[0]);
        self.render_ai_activity(frame, chunks[2]);
        if self.ai_done {
            self.render_ai_buttons(frame, chunks[4]);
        }
    }

    fn render_ai_activity(&self, frame: &mut Frame, area: Rect) {
        let title = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "AI Activity",
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::INFO))
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let visible_rows = inner.height as usize;
        if visible_rows == 0 {
            return;
        }
        let lines: Vec<Line<'static>> = if self.ai_log.is_empty() {
            vec![Line::from(Span::styled(
                "Waiting for AI to start working on the conflicts...",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ))]
        } else {
            let max_scroll = self.ai_log.len().saturating_sub(visible_rows) as u16;
            let scroll = self.ai_scroll.min(max_scroll) as usize;
            let end = self.ai_log.len().saturating_sub(scroll);
            let start = end.saturating_sub(visible_rows);
            self.ai_log[start..end]
                .iter()
                .map(ai_activity_event_to_line)
                .collect()
        };
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_ai_buttons(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(16),
                Constraint::Length(2),
                Constraint::Length(16),
                Constraint::Min(0),
            ])
            .split(area);

        frame.render_widget(
            button_paragraph(
                " Complete ",
                colors::SUCCESS,
                matches!(self.ai_button, AiButton::Complete),
            ),
            chunks[1],
        );
        frame.render_widget(
            button_paragraph(
                "  Cancel  ",
                colors::ERROR,
                matches!(self.ai_button, AiButton::Cancel),
            ),
            chunks[3],
        );
    }
}

fn button_paragraph(label: &str, color: ratatui::style::Color, focused: bool) -> Paragraph<'static> {
    let border_style = if focused {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style);
    let label_style = if focused {
        Style::default()
            .fg(color)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    };
    Paragraph::new(Line::from(Span::styled(label.to_string(), label_style)))
        .block(block)
        .alignment(ratatui::layout::Alignment::Center)
}

fn ai_activity_event_to_line(event: &AiActivityEvent) -> Line<'static> {
    match event {
        AiActivityEvent::SessionStart { model } => Line::from(vec![
            Span::styled("[session started] ".to_string(), muted_bold()),
            Span::styled("model".to_string(), muted_dim()),
            Span::styled(": ".to_string(), muted_dim()),
            Span::styled(
                model.clone(),
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        AiActivityEvent::AssistantText { content } => Line::from(vec![
            Span::styled(
                "AI".to_string(),
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": ".to_string(), muted_dim()),
            Span::styled(content.clone(), Style::default().fg(colors::WHITE)),
        ]),
        AiActivityEvent::Thinking { content } => Line::from(vec![
            Span::styled(
                "Thinking".to_string(),
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD | Modifier::ITALIC),
            ),
            Span::styled(": ".to_string(), muted_dim()),
            Span::styled(
                content.clone(),
                Style::default()
                    .fg(colors::GRAY_LIGHT)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]),
        AiActivityEvent::ToolCall { tool_name, summary } => {
            let mut spans = vec![
                Span::styled("> ".to_string(), Style::default().fg(colors::INFO)),
                Span::styled(
                    tool_name.clone(),
                    Style::default()
                        .fg(colors::SUCCESS)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("(".to_string(), muted_dim()),
            ];
            push_highlighted_fragment(&mut spans, summary);
            spans.push(Span::styled(")".to_string(), muted_dim()));
            Line::from(spans)
        }
        AiActivityEvent::ToolResult {
            tool_name,
            status,
            detail,
        } => {
            let (label, label_style) = match status {
                AiToolResultStatus::Success => (
                    "ok",
                    Style::default()
                        .fg(colors::SUCCESS)
                        .add_modifier(Modifier::BOLD),
                ),
                AiToolResultStatus::Error => (
                    "error",
                    Style::default()
                        .fg(colors::ERROR)
                        .add_modifier(Modifier::BOLD),
                ),
            };
            let mut spans = vec![Span::styled(
                "< ".to_string(),
                Style::default().fg(match status {
                    AiToolResultStatus::Success => colors::SUCCESS,
                    AiToolResultStatus::Error => colors::ERROR,
                }),
            )];
            if let Some(tool_name) = tool_name {
                spans.push(Span::styled(
                    tool_name.clone(),
                    Style::default()
                        .fg(colors::INFO)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::raw(" ".to_string()));
            }
            spans.push(Span::styled(label.to_string(), label_style));
            spans.push(Span::styled(": ".to_string(), muted_dim()));
            push_highlighted_fragment(&mut spans, detail);
            Line::from(spans)
        }
        AiActivityEvent::Notice { severity, message } => Line::from(vec![
            Span::styled(
                match severity {
                    AiActivitySeverity::Info => "info",
                    AiActivitySeverity::Warning => "warning",
                    AiActivitySeverity::Error => "error",
                }
                .to_string(),
                Style::default()
                    .fg(match severity {
                        AiActivitySeverity::Info => colors::INFO,
                        AiActivitySeverity::Warning => colors::WARNING,
                        AiActivitySeverity::Error => colors::ERROR,
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": ".to_string(), muted_dim()),
            Span::styled(
                message.clone(),
                Style::default().fg(match severity {
                    AiActivitySeverity::Info => colors::WHITE,
                    AiActivitySeverity::Warning => colors::WARNING,
                    AiActivitySeverity::Error => colors::ERROR,
                }),
            ),
        ]),
        AiActivityEvent::Summary {
            tool_calls,
            duration_ms,
            total_tokens,
        } => Line::from(vec![
            Span::styled(
                "[done] ".to_string(),
                Style::default()
                    .fg(colors::SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(tool_calls.to_string(), Style::default().fg(colors::ACCENT)),
            Span::styled(" tools · ".to_string(), muted_dim()),
            Span::styled(
                format!("{:.1}s", *duration_ms as f64 / 1000.0),
                Style::default().fg(colors::ACCENT),
            ),
            Span::styled(" · ".to_string(), muted_dim()),
            Span::styled(
                total_tokens.to_string(),
                Style::default().fg(colors::ACCENT),
            ),
            Span::styled(" tokens".to_string(), muted_dim()),
        ]),
        AiActivityEvent::Raw { text } => Line::from(Span::styled(
            text.clone(),
            Style::default().fg(colors::GRAY_LIGHT),
        )),
    }
}

fn push_highlighted_fragment(spans: &mut Vec<Span<'static>>, fragment: &str) {
    let mut chars = fragment.chars().peekable();
    let mut first_token = true;
    while let Some(ch) = chars.peek().copied() {
        if ch.is_whitespace() {
            let mut ws = String::new();
            while let Some(next) = chars.peek().copied() {
                if !next.is_whitespace() {
                    break;
                }
                ws.push(next);
                chars.next();
            }
            spans.push(Span::raw(ws));
            continue;
        }

        let mut token = String::new();
        let mut quote: Option<char> = None;
        while let Some(next) = chars.peek().copied() {
            if let Some(active_quote) = quote {
                token.push(next);
                chars.next();
                if next == active_quote {
                    quote = None;
                }
                continue;
            }
            if next.is_whitespace() {
                break;
            }
            if matches!(next, '"' | '\'' | '`') {
                quote = Some(next);
            }
            token.push(next);
            chars.next();
        }

        push_highlighted_token(spans, &token, &mut first_token);
    }
}

fn push_highlighted_token(spans: &mut Vec<Span<'static>>, token: &str, first_token: &mut bool) {
    if let Some((lhs, rhs)) = token.split_once('=') {
        if is_assignment_like(lhs) {
            spans.push(Span::styled(
                lhs.to_string(),
                if lhs.starts_with('-') {
                    Style::default().fg(colors::BRAND)
                } else {
                    Style::default().fg(colors::INFO)
                },
            ));
            spans.push(Span::styled("=".to_string(), muted_dim()));
            if !rhs.is_empty() {
                spans.push(Span::styled(
                    rhs.to_string(),
                    classify_token_style(rhs, false),
                ));
            }
            *first_token = false;
            return;
        }
    }

    spans.push(Span::styled(
        token.to_string(),
        classify_token_style(token, *first_token),
    ));
    *first_token = false;
}

fn classify_token_style(token: &str, first_token: bool) -> Style {
    if is_shell_operator(token) {
        return Style::default().fg(colors::INFO);
    }
    if token.starts_with("--") || (token.starts_with('-') && token.len() > 1) {
        return Style::default().fg(colors::BRAND);
    }
    if is_quoted(token) || is_placeholder(token) {
        return Style::default().fg(colors::WARNING);
    }
    if looks_like_url(token) {
        return Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::UNDERLINED);
    }
    if looks_like_path(token) {
        return Style::default().fg(colors::EMPHASIS);
    }
    if looks_like_number(token) {
        return Style::default().fg(colors::ACCENT);
    }
    if first_token {
        return Style::default()
            .fg(colors::SUCCESS)
            .add_modifier(Modifier::BOLD);
    }
    Style::default().fg(colors::WHITE)
}

fn is_assignment_like(token: &str) -> bool {
    token.starts_with('-')
        || token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
}

fn is_shell_operator(token: &str) -> bool {
    matches!(token, "&&" | "||" | "|" | ";" | "->" | "=>")
}

fn is_quoted(token: &str) -> bool {
    token.len() >= 2
        && ((token.starts_with('"') && token.ends_with('"'))
            || (token.starts_with('\'') && token.ends_with('\''))
            || (token.starts_with('`') && token.ends_with('`')))
}

fn is_placeholder(token: &str) -> bool {
    token.starts_with('<') && token.ends_with('>')
}

fn looks_like_url(token: &str) -> bool {
    token.starts_with("http://") || token.starts_with("https://")
}

fn looks_like_path(token: &str) -> bool {
    let trimmed = token
        .trim_matches(|ch: char| matches!(ch, ',' | ':' | ';' | '(' | ')' | '[' | ']' | '{' | '}'));
    trimmed.starts_with("~/")
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || trimmed.starts_with('/')
        || trimmed.contains('/')
        || trimmed.ends_with(".rs")
        || trimmed.ends_with(".rb")
        || trimmed.ends_with(".ts")
        || trimmed.ends_with(".tsx")
        || trimmed.ends_with(".js")
        || trimmed.ends_with(".json")
        || trimmed.ends_with(".md")
}

fn looks_like_number(token: &str) -> bool {
    token.chars().any(|ch| ch.is_ascii_digit())
        && token.chars().all(|ch| {
            ch.is_ascii_digit() || matches!(ch, '.' | '%' | ':' | '+' | '-' | '_' | 's' | 'm')
        })
}

fn muted_dim() -> Style {
    Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM)
}

fn muted_bold() -> Style {
    Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::BOLD)
}

fn build_detail_lines(request: &UpdatePullRequestRequest) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();

    rows.push(labeled_line(
        "PR",
        Span::styled(
            format!("#{} ", request.number),
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        ),
        Some(Span::styled(
            "(Open)".to_string(),
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::DIM),
        )),
    ));

    rows.push(labeled_line(
        "Title",
        Span::styled(
            request.title.clone(),
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
        None,
    ));

    rows.push(labeled_line(
        "URL",
        Span::styled(request.url.clone(), Style::default().fg(colors::EMPHASIS)),
        None,
    ));

    rows.push(labeled_line(
        "Branch",
        Span::styled(
            request.branch.clone(),
            Style::default()
                .fg(colors::SUCCESS)
                .add_modifier(Modifier::BOLD),
        ),
        None,
    ));

    rows.push(labeled_line(
        "Worktree",
        Span::styled(
            request.worktree_path.clone(),
            Style::default().fg(colors::EMPHASIS),
        ),
        None,
    ));

    rows.push(labeled_line(
        "Base ref",
        Span::styled(
            request
                .base_ref
                .clone()
                .unwrap_or_else(|| "(resolving...)".to_string()),
            Style::default()
                .fg(colors::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        None,
    ));

    rows.push(labeled_line(
        "Behind",
        Span::styled(
            format!("-{}", request.behind),
            Style::default()
                .fg(colors::ERROR)
                .add_modifier(Modifier::BOLD),
        ),
        None,
    ));

    if request.ahead > 0 {
        rows.push(labeled_line(
            "Ahead",
            Span::styled(
                format!("+{}", request.ahead),
                Style::default().fg(colors::SUCCESS),
            ),
            None,
        ));
    }

    rows
}

fn build_steps_lines(base_ref: &str) -> Vec<Line<'static>> {
    let header_style = Style::default()
        .fg(colors::INFO)
        .add_modifier(Modifier::BOLD);
    let bullet_style = Style::default().fg(colors::EMPHASIS);
    let muted = Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM);
    vec![
        Line::from(Span::styled("Will run:".to_string(), header_style)),
        Line::from(vec![
            Span::styled("  • ".to_string(), muted),
            Span::styled("git fetch --all --prune".to_string(), bullet_style),
        ]),
        Line::from(vec![
            Span::styled("  • ".to_string(), muted),
            Span::styled(format!("git merge {base_ref}"), bullet_style),
        ]),
        Line::from(vec![
            Span::styled("  • ".to_string(), muted),
            Span::styled(
                "on conflict: opencode streams resolution, then Complete/Cancel".to_string(),
                bullet_style,
            ),
        ]),
        Line::from(vec![
            Span::styled("  • ".to_string(), muted),
            Span::styled("git push origin HEAD".to_string(), bullet_style),
        ]),
    ]
}

fn labeled_line(
    label: &str,
    value: Span<'static>,
    trailing: Option<Span<'static>>,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
    spans.push(Span::styled(
        format!("{label:<12}"),
        Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM),
    ));
    spans.push(value);
    if let Some(extra) = trailing {
        spans.push(extra);
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_dump(screen: &UpdatePullRequestScreen, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| screen.render(f, f.area())).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn sample_request() -> UpdatePullRequestRequest {
        UpdatePullRequestRequest {
            number: 21,
            title: "Improve onboarding flow".to_string(),
            url: "https://github.com/example/repo/pull/21".to_string(),
            branch: "feat-onboarding".to_string(),
            worktree_path: "/tmp/repo-onboarding".to_string(),
            ahead: 4,
            behind: 7,
            base_ref: None,
        }
    }

    #[test]
    fn screen_starts_in_loading_when_base_ref_unknown() {
        let screen = UpdatePullRequestScreen::new(sample_request());
        assert_eq!(screen.step(), UpdateStep::Loading);
        assert!(screen.error().is_none());
    }

    #[test]
    fn set_base_ref_transitions_to_confirm_default_no() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        assert_eq!(screen.step(), UpdateStep::Confirm);
        let dialog = screen
            .confirm
            .as_ref()
            .expect("confirm built after base ref");
        assert_eq!(dialog.selected, ConfirmChoice::Cancel);
        assert_eq!(dialog.confirm_label, "Yes");
        assert_eq!(dialog.cancel_label, "No");
        assert_eq!(dialog.variant, ConfirmVariant::Default);
    }

    #[test]
    fn enter_on_no_returns_cancelled() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::Cancelled);
    }

    #[test]
    fn tab_then_enter_confirms() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(tab), UpdateAction::Continue);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::Confirmed);
    }

    #[test]
    fn esc_during_loading_cancels() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), UpdateAction::Cancelled);
    }

    #[test]
    fn keys_are_ignored_while_updating_before_ai_done() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), UpdateAction::Continue);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::Continue);
        assert!(screen.is_updating());
    }

    #[test]
    fn set_error_clears_confirm_and_any_key_dismisses() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_error("boom".to_string());
        let any = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(screen.handle_key(any), UpdateAction::Cancelled);
    }

    #[test]
    fn enter_on_complete_returns_ai_complete() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        screen.mark_ai_done();
        assert_eq!(screen.ai_button(), AiButton::Complete);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::AiComplete);
    }

    #[test]
    fn right_then_enter_returns_ai_cancel() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        screen.mark_ai_done();
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(right), UpdateAction::Continue);
        assert_eq!(screen.ai_button(), AiButton::Cancel);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::AiCancel);
    }

    #[test]
    fn esc_after_ai_done_returns_ai_cancel() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        screen.mark_ai_done();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), UpdateAction::AiCancel);
    }

    #[test]
    fn ai_log_truncates_at_cap() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        for i in 0..(AI_LOG_MAX_LINES + 50) {
            screen.append_ai_line(format!("line {i}"));
        }
        let lines = screen.ai_log_lines();
        assert_eq!(lines.len(), AI_LOG_MAX_LINES);
        assert_eq!(
            lines.last().unwrap(),
            &format!("line {}", AI_LOG_MAX_LINES + 49)
        );
    }

    #[test]
    fn assistant_activity_deltas_coalesce_into_one_row() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.append_ai_line(AiActivityEvent::AssistantText {
            content: "git".to_string(),
        });
        screen.append_ai_line(AiActivityEvent::AssistantText {
            content: " status".to_string(),
        });

        assert_eq!(screen.ai_log_lines(), vec!["AI: git status".to_string()]);
    }

    #[test]
    fn tool_call_activity_uses_monokai_style_roles() {
        let line = ai_activity_event_to_line(&AiActivityEvent::ToolCall {
            tool_name: "run_shell_command".to_string(),
            summary: "git diff -- src/main.rs --color=never".to_string(),
        });

        assert!(
            line.spans
                .iter()
                .any(|span| span.style.fg == Some(colors::SUCCESS)),
            "expected command token highlighting: {line:?}"
        );
        assert!(
            line.spans
                .iter()
                .any(|span| span.style.fg == Some(colors::BRAND)),
            "expected flag highlighting: {line:?}"
        );
        assert!(
            line.spans
                .iter()
                .any(|span| span.style.fg == Some(colors::EMPHASIS)),
            "expected path highlighting: {line:?}"
        );
    }

    #[test]
    fn phase_message_updates_during_updating() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.set_phase_message("Conflict found — opencode resolving...");
        assert_eq!(
            screen.phase_message(),
            "Conflict found — opencode resolving..."
        );
    }

    #[test]
    fn mouse_wheel_scrolls_ai_activity_when_ai_active() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();

        assert_eq!(screen.ai_scroll(), 0);
        assert!(screen.handle_mouse_scroll_up(3));
        assert_eq!(screen.ai_scroll(), 3);
        assert!(screen.handle_mouse_scroll_down(2));
        assert_eq!(screen.ai_scroll(), 1);
        assert!(screen.handle_mouse_scroll_down(9));
        assert_eq!(screen.ai_scroll(), 0);
    }

    #[test]
    fn ai_activity_scroll_stays_stable_when_new_lines_arrive() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        for i in 0..10 {
            screen.append_ai_line(format!("line {i}"));
        }

        assert!(screen.handle_mouse_scroll_up(4));
        assert_eq!(screen.ai_scroll(), 4);

        screen.append_ai_line("line 10");
        assert_eq!(screen.ai_scroll(), 5);
    }

    #[test]
    fn ai_activity_panel_hidden_until_marked_active() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        assert!(!screen.ai_active());

        let before = render_dump(&screen, 100, 24);
        assert!(
            !before.contains("AI Activity"),
            "AI Activity panel rendered before conflicts detected:\n{before}"
        );

        screen.mark_ai_active();
        assert!(screen.ai_active());
        let after = render_dump(&screen, 100, 24);
        assert!(
            after.contains("AI Activity"),
            "AI Activity panel missing after mark_ai_active:\n{after}"
        );
    }

    #[test]
    fn complete_and_cancel_buttons_visible_after_ai_done() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        screen.mark_ai_done();
        assert!(screen.ai_done());
        let dumped = render_dump(&screen, 100, 28);
        assert!(
            dumped.contains("Complete"),
            "expected Complete button:\n{dumped}"
        );
        assert!(
            dumped.contains("Cancel"),
            "expected Cancel button:\n{dumped}"
        );
    }

    #[test]
    fn updating_height_is_compact_until_ai_active() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        let compact = screen.preferred_content_height();
        screen.mark_ai_active();
        let expanded = screen.preferred_content_height();
        assert!(
            expanded > compact,
            "expected AI-active height ({expanded}) to exceed pre-AI height ({compact})"
        );
    }

    #[test]
    fn start_updating_resets_ai_active_flag() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        assert!(screen.ai_active());
        screen.mark_ai_done();
        assert!(screen.ai_done());
        screen.start_updating();
        assert!(!screen.ai_active());
        assert!(!screen.ai_done());
    }

    #[test]
    fn render_confirm_shows_base_ref_and_buttons() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());

        let dumped = render_dump(&screen, 100, 28);

        assert!(
            dumped.contains("Update Pull Request #21?"),
            "expected title in:\n{dumped}"
        );
        assert!(dumped.contains("upstream/main"));
        assert!(dumped.contains("-7"));
        assert!(dumped.contains("Yes"));
        assert!(dumped.contains("No"));
    }
}

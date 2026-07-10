//! Monokai-styled renderer for a non-interactive `opencode run` transcript.
//!
//! The Bugkill investigation embeds `opencode run` (read-only Plan agent) in
//! a PTY, and that mode prints an almost unstyled line grammar: a
//! `> agent · model` header, one `<icon> <title> · <description>` line per
//! tool call, and plain-text assistant paragraphs — rendered verbatim it is
//! a wall of default-colored text. This widget re-creates the look of
//! opencode's own TUI with the Monokai theme by classifying each line and
//! applying the colors opencode's interactive renderer would: completed tool
//! rows muted, assistant text in the foreground color, thinking in faded
//! yellow, errors in pink, all on the Monokai background.
//!
//! The caller passes the full ANSI-stripped transcript every frame; the
//! widget classifies + word-wraps it and owns only the scroll offset (same
//! clamping contract as `vt100::Parser::set_scrollback`: any offset is
//! clamped to the available scrollback at render time).

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

// ── Monokai palette (mirrors opencode's monokai.json, dark variant) ─────
const BACKGROUND: Color = Color::Rgb(0x27, 0x28, 0x22);
const FOREGROUND: Color = Color::Rgb(0xf8, 0xf8, 0xf2);
const MUTED: Color = Color::Rgb(0x75, 0x71, 0x5e);
const CYAN: Color = Color::Rgb(0x66, 0xd9, 0xef);
const PINK: Color = Color::Rgb(0xf9, 0x26, 0x72);
const YELLOW: Color = Color::Rgb(0xe6, 0xdb, 0x74);
/// opencode renders thinking headers at 0.6 opacity over the background;
/// this is Monokai yellow pre-blended the same way.
const YELLOW_FADED: Color = Color::Rgb(0x9a, 0x93, 0x53);

/// Leading glyphs `opencode run` puts on completed tool lines (Read, Glob,
/// Grep, Edit, WebFetch, WebSearch, bash/task/todo blocks, subagents, LSP).
const TOOL_ICONS: [char; 10] = ['→', '✱', '←', '%', '◈', '#', '$', '•', '✓', '⚙'];

#[derive(Default)]
pub struct RunTranscriptView {
    /// Wrapped lines scrolled back from the live tail; 0 = follow the tail.
    offset: usize,
}

impl RunTranscriptView {
    pub fn scroll_up(&mut self, lines: u16) {
        self.offset = self.offset.saturating_add(lines as usize);
    }

    pub fn scroll_down(&mut self, lines: u16) {
        self.offset = self.offset.saturating_sub(lines as usize);
    }

    pub fn scroll_to_top(&mut self) {
        self.offset = usize::MAX;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.offset = 0;
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, transcript: &str) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let lines = build_lines(transcript, area.width as usize);
        let total = lines.len();
        let height = area.height as usize;
        let max_offset = total.saturating_sub(height);
        self.offset = self.offset.min(max_offset);
        let start = max_offset - self.offset;
        let visible: Vec<Line<'static>> = lines[start..(start + height).min(total)].to_vec();
        frame.render_widget(
            Paragraph::new(visible).style(Style::default().fg(FOREGROUND).bg(BACKGROUND)),
            area,
        );
        if max_offset > 0 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(Style::default().fg(MUTED))
                .thumb_style(Style::default().fg(CYAN));
            let mut state = ScrollbarState::new(total)
                .viewport_content_length(height)
                .position(start);
            frame.render_stateful_widget(scrollbar, area, &mut state);
        }
    }
}

/// Classify one logical transcript line into styled segments, following the
/// color model of opencode's interactive scrollback (Monokai values).
fn classify(line: &str) -> Vec<(String, Style)> {
    let text = Style::default().fg(FOREGROUND);
    let muted = Style::default().fg(MUTED);
    if line.is_empty() {
        return Vec::new();
    }

    // `> plan · gpt-5.5` run header.
    if let Some(rest) = line.strip_prefix("> ") {
        let mut segments = vec![("> ".to_string(), Style::default().fg(CYAN))];
        match rest.split_once(" · ") {
            Some((agent, model)) => {
                segments.push((agent.to_string(), text.add_modifier(Modifier::BOLD)));
                segments.push((format!(" · {model}"), muted));
            }
            None => segments.push((rest.to_string(), text.add_modifier(Modifier::BOLD))),
        }
        return segments;
    }

    // `UI.error` output: danger-bold prefix, normal message.
    if let Some(rest) = line.strip_prefix("Error: ") {
        return vec![
            (
                "Error: ".to_string(),
                Style::default().fg(PINK).add_modifier(Modifier::BOLD),
            ),
            (rest.to_string(), text),
        ];
    }

    // Reasoning (only present with `--thinking`).
    if line.starts_with("Thinking:") {
        return vec![(
            line.to_string(),
            Style::default()
                .fg(YELLOW_FADED)
                .add_modifier(Modifier::ITALIC),
        )];
    }

    // Todo-block body rows.
    for (mark, style) in [
        ("[✓] ", muted.add_modifier(Modifier::CROSSED_OUT)),
        ("[•] ", text),
        ("[ ] ", muted),
    ] {
        if line.starts_with(mark) {
            return vec![(line.to_string(), style)];
        }
    }

    // Icon-led lines: failures pink, permission warnings yellow, share
    // URLs cyan, completed tool rows muted (whole line, like the TUI).
    let mut chars = line.chars();
    let first = chars.next().expect("line is non-empty");
    if matches!(chars.next(), Some(' ') | None) {
        let style = match first {
            '✗' | '✖' => Some(Style::default().fg(PINK)),
            '!' => Some(Style::default().fg(YELLOW)),
            '~' => Some(Style::default().fg(CYAN)),
            c if TOOL_ICONS.contains(&c) => Some(muted),
            _ => None,
        };
        if let Some(style) = style {
            return vec![(line.to_string(), style)];
        }
    }

    // Everything else is assistant text.
    vec![(line.to_string(), text)]
}

fn flatten(segments: Vec<(String, Style)>) -> Vec<(char, Style)> {
    let mut flat = Vec::new();
    for (text, style) in segments {
        for ch in text.chars() {
            match ch {
                '\t' => flat.extend([(' ', style), (' ', style)]),
                _ => flat.push((ch, style)),
            }
        }
    }
    flat
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Word-wrap a styled character stream to `width` cells. Breaks at the last
/// space when possible, hard-splits over-long words, and drops the space
/// that would otherwise start a continuation line.
fn wrap_chars(chars: Vec<(char, Style)>, width: usize) -> Vec<Vec<(char, Style)>> {
    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut used = 0usize;
    for (ch, style) in chars {
        if ch == ' ' && current.is_empty() && !lines.is_empty() {
            continue;
        }
        let w = char_width(ch);
        if used + w > width && !current.is_empty() {
            let tail = match current.iter().rposition(|&(c, _)| c == ' ') {
                Some(idx) => {
                    let tail = current.split_off(idx + 1);
                    current.pop(); // the break space itself
                    tail
                }
                None => Vec::new(),
            };
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            current = tail;
            used = current.iter().map(|&(c, _)| char_width(c)).sum();
            if ch == ' ' && current.is_empty() {
                continue;
            }
        }
        current.push((ch, style));
        used += w;
    }
    lines.push(current);
    lines
}

/// Group a wrapped character run back into spans (adjacent equal styles
/// merge) so ratatui renders it in one pass.
fn to_line(chars: Vec<(char, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut run_style: Option<Style> = None;
    for (ch, style) in chars {
        match run_style {
            Some(current) if current == style => buf.push(ch),
            Some(current) => {
                spans.push(Span::styled(std::mem::take(&mut buf), current));
                run_style = Some(style);
                buf.push(ch);
            }
            None => {
                run_style = Some(style);
                buf.push(ch);
            }
        }
    }
    if let Some(style) = run_style {
        if !buf.is_empty() {
            spans.push(Span::styled(buf, style));
        }
    }
    Line::from(spans)
}

fn build_lines(transcript: &str, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    transcript
        .lines()
        .flat_map(|raw| {
            wrap_chars(flatten(classify(raw)), width)
                .into_iter()
                .map(to_line)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn spans_of(transcript: &str, width: usize) -> Vec<Vec<(String, Style)>> {
        build_lines(transcript, width)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| (span.content.into_owned(), span.style))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn lines_get_their_monokai_colors() {
        let transcript = "→ Read src/main.rs [offset=1]\n\nThe bug is in the parser.";
        let lines = spans_of(transcript, 80);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].len(), 1);
        assert_eq!(lines[0][0].1.fg, Some(MUTED));
        assert!(lines[1].is_empty());
        assert_eq!(lines[2][0].1.fg, Some(FOREGROUND));
    }

    #[test]
    fn header_splits_agent_and_model() {
        let lines = spans_of("> plan · gpt-5.5", 80);
        let header = &lines[0];
        assert_eq!(header.len(), 3);
        assert_eq!(header[0], ("> ".to_string(), Style::default().fg(CYAN)));
        assert_eq!(header[1].0, "plan");
        assert!(header[1].1.add_modifier.contains(Modifier::BOLD));
        assert_eq!(header[2].0, " · gpt-5.5");
        assert_eq!(header[2].1.fg, Some(MUTED));
    }

    #[test]
    fn error_warning_thinking_and_todo_lines() {
        let transcript = "✗ grep failed\n\
                          ! permission requested: edit; auto-rejecting\n\
                          Thinking: hmm\n\
                          Error: boom\n\
                          [✓] done item";
        let lines = spans_of(transcript, 200);
        assert_eq!(lines[0][0].1.fg, Some(PINK));
        assert_eq!(lines[1][0].1.fg, Some(YELLOW));
        assert_eq!(lines[2][0].1.fg, Some(YELLOW_FADED));
        assert!(lines[2][0].1.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(lines[3][0].0, "Error: ");
        assert_eq!(lines[3][0].1.fg, Some(PINK));
        assert_eq!(lines[3][1].1.fg, Some(FOREGROUND));
        assert!(lines[4][0].1.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn long_tool_lines_word_wrap_keeping_style() {
        let lines = spans_of("✱ Grep \"pattern\" in src · 12 matches", 14);
        assert!(lines.len() > 1);
        for line in &lines {
            let text: String = line.iter().map(|(s, _)| s.as_str()).collect();
            assert!(text.chars().count() <= 14, "overflow: {text:?}");
            assert!(!text.starts_with(' '));
            assert_eq!(line[0].1.fg, Some(MUTED));
        }
        let rejoined: Vec<String> = lines
            .iter()
            .map(|line| line.iter().map(|(s, _)| s.as_str()).collect())
            .collect();
        assert_eq!(rejoined.join(" "), "✱ Grep \"pattern\" in src · 12 matches");
    }

    #[test]
    fn long_words_hard_break() {
        let lines = spans_of("abcdefghij", 5);
        let texts: Vec<String> = lines
            .iter()
            .map(|line| line.iter().map(|(s, _)| s.as_str()).collect())
            .collect();
        assert_eq!(texts, ["abcde", "fghij"]);
    }

    #[test]
    fn scroll_offsets_clamp_and_follow_the_tail() {
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = RunTranscriptView::default();
        let transcript = "a\nb\nc\nd\ne";
        let area = Rect::new(0, 0, 10, 3);

        view.scroll_to_top();
        terminal
            .draw(|frame| view.render(frame, area, transcript))
            .expect("draw");
        let top = terminal.backend().buffer()[(0, 0)].clone();
        assert_eq!(top.symbol(), "a");
        assert_eq!(top.bg, BACKGROUND);

        view.scroll_to_bottom();
        terminal
            .draw(|frame| view.render(frame, area, transcript))
            .expect("draw");
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "c");

        // Over-scrolling down past the tail stays at the tail.
        view.scroll_down(50);
        terminal
            .draw(|frame| view.render(frame, area, transcript))
            .expect("draw");
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "c");
    }
}

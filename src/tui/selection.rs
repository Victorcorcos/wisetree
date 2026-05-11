//! Mouse-driven text selection over the rendered terminal buffer.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;

use crate::messages::colors;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseSelection {
    anchor: Position,
    focus: Position,
    dragged: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectionRange {
    start: Position,
    end: Position,
}

pub struct SelectionOverlay<'a> {
    snapshot: &'a Buffer,
    selection: &'a MouseSelection,
}

impl MouseSelection {
    pub fn start(position: Position) -> Self {
        Self {
            anchor: position,
            focus: position,
            dragged: false,
        }
    }

    pub fn update(&mut self, position: Position) {
        self.dragged |= position != self.anchor;
        self.focus = position;
    }

    fn range(&self) -> Option<SelectionRange> {
        if !self.dragged || self.anchor == self.focus {
            return None;
        }

        Some(if comes_before(self.anchor, self.focus) {
            SelectionRange {
                start: self.anchor,
                end: self.focus,
            }
        } else {
            SelectionRange {
                start: self.focus,
                end: self.anchor,
            }
        })
    }
}

impl<'a> SelectionOverlay<'a> {
    pub fn new(snapshot: &'a Buffer, selection: &'a MouseSelection) -> Self {
        Self {
            snapshot,
            selection,
        }
    }
}

impl Widget for SelectionOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let Some(range) = self.selection.range() else {
            return;
        };

        let left = area
            .left()
            .max(self.snapshot.area.left())
            .max(range.start.x);
        let right = area
            .right()
            .saturating_sub(1)
            .min(self.snapshot.area.right().saturating_sub(1))
            .min(range.end.x.max(range.start.x));
        let top = area.top().max(self.snapshot.area.top()).max(range.start.y);
        let bottom = area
            .bottom()
            .saturating_sub(1)
            .min(self.snapshot.area.bottom().saturating_sub(1))
            .max(top)
            .min(range.end.y.max(range.start.y));

        for y in top..=bottom {
            let row_start = if y == range.start.y {
                range.start.x
            } else {
                self.snapshot.area.left()
            };
            let row_end = if y == range.end.y {
                range.end.x
            } else {
                self.snapshot.area.right().saturating_sub(1)
            };

            for x in row_start.max(left)..=row_end.min(right) {
                let source = &self.snapshot[(x, y)];
                let target = &mut buf[(x, y)];
                target.set_symbol(source.symbol()).set_style(
                    Style::default()
                        .fg(colors::WHITE)
                        .bg(colors::BG_FOCUS)
                        .add_modifier(Modifier::BOLD),
                );
            }
        }
    }
}

pub fn clamp_position(position: Position, area: Rect) -> Option<Position> {
    if area.width == 0 || area.height == 0 {
        return None;
    }

    let min_x = area.left();
    let max_x = area.right().saturating_sub(1);
    let min_y = area.top();
    let max_y = area.bottom().saturating_sub(1);

    Some(Position {
        x: position.x.clamp(min_x, max_x),
        y: position.y.clamp(min_y, max_y),
    })
}

pub fn contains_position(area: Rect, position: Position) -> bool {
    position.x >= area.left()
        && position.x < area.right()
        && position.y >= area.top()
        && position.y < area.bottom()
}

pub fn extract_text(snapshot: &Buffer, selection: &MouseSelection) -> Option<String> {
    let range = selection.range()?;
    let mut lines = Vec::new();

    for y in range.start.y..=range.end.y {
        let start_x = if y == range.start.y {
            range.start.x
        } else {
            snapshot.area.left()
        };
        let end_x = if y == range.end.y {
            range.end.x
        } else {
            snapshot.area.right().saturating_sub(1)
        };

        let mut line = String::new();
        for x in start_x..=end_x {
            if !contains_position(snapshot.area, Position { x, y }) {
                continue;
            }
            line.push_str(snapshot[(x, y)].symbol());
        }
        lines.push(line.trim_end_matches(' ').to_string());
    }

    let text = lines.join("\n");
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn comes_before(left: Position, right: Position) -> bool {
    left.y < right.y || (left.y == right.y && left.x <= right.x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    #[test]
    fn extract_text_returns_trimmed_multiline_selection() {
        let snapshot = Buffer::with_lines(["Hello World   ", "Second line  "]);
        let mut selection = MouseSelection::start(Position { x: 0, y: 0 });
        selection.update(Position { x: 5, y: 1 });

        let text = extract_text(&snapshot, &selection).expect("selection text");
        assert_eq!(text, "Hello World\nSecond");
    }

    #[test]
    fn extract_text_ignores_click_without_drag() {
        let snapshot = Buffer::with_lines(["Hello"]);
        let selection = MouseSelection::start(Position { x: 0, y: 0 });
        assert!(extract_text(&snapshot, &selection).is_none());
    }

    #[test]
    fn overlay_highlights_selected_cells() {
        let snapshot = Buffer::with_lines(["Hello"]);
        let area = Rect::new(0, 0, 5, 1);
        let mut target = snapshot.clone();
        let mut selection = MouseSelection::start(Position { x: 0, y: 0 });
        selection.update(Position { x: 1, y: 0 });

        SelectionOverlay::new(&snapshot, &selection).render(area, &mut target);

        assert_eq!(target[(0, 0)].bg, colors::BG_FOCUS);
        assert_eq!(target[(1, 0)].bg, colors::BG_FOCUS);
        assert_eq!(target[(2, 0)].bg, snapshot[(2, 0)].bg);
    }
}

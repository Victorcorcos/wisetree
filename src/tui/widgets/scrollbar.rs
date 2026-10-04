//! Shared vertical scrollbar for tail-anchored views (embedded PTYs, the
//! create-flow Terminal Activity log, …). Centralizing the math keeps every
//! inner terminal's scrollbar accurate and consistent.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;

use crate::messages::colors;

/// Map a *tail-anchored* scroll model onto ratatui's
/// `(content_length, position, viewport_content_length)` scrollbar state.
///
/// - `scrollable_rows` — how many rows of history sit above the viewport when
///   it's pinned to the bottom (vt100 scrollback length, or `lines - viewport`
///   for a line log).
/// - `offset_from_bottom` — how far the view is scrolled up from the live tail
///   (`0` = at the bottom).
///
/// Returns `None` when there's nothing to scroll.
///
/// The crux of the fix: ratatui parks the thumb flush at the bottom of the
/// track **only** when `position == content_length - 1`. At the tail
/// (`offset_from_bottom == 0`) `position == scrollable_rows`, so
/// `content_length = scrollable_rows + 1` makes that the maximum and the thumb
/// sits exactly at the bottom. The previous `scrollable_rows + viewport`
/// content length left the thumb floating `viewport - 1` rows short of the
/// bottom even when fully scrolled down. `viewport_content_length` keeps the
/// thumb sized to the visible fraction of the content.
pub fn scrollbar_metrics(
    scrollable_rows: usize,
    offset_from_bottom: usize,
    viewport: usize,
) -> Option<(usize, usize, usize)> {
    if scrollable_rows == 0 {
        return None;
    }
    let content_length = scrollable_rows.saturating_add(1);
    let position = scrollable_rows.saturating_sub(offset_from_bottom);
    Some((content_length, position, viewport))
}

/// Render a right-aligned vertical scrollbar for a tail-anchored view (see
/// [`scrollbar_metrics`]). The track spans `area`; the viewport length is the
/// area height. No-op when there's nothing to scroll.
pub fn render_vertical_scrollbar(
    frame: &mut Frame,
    area: Rect,
    scrollable_rows: usize,
    offset_from_bottom: usize,
) {
    let Some((content_length, position, viewport)) =
        scrollbar_metrics(scrollable_rows, offset_from_bottom, area.height as usize)
    else {
        return;
    };
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .style(Style::default().fg(colors::MUTED))
        .thumb_style(Style::default().fg(colors::INFO));
    let mut state = ScrollbarState::new(content_length)
        .viewport_content_length(viewport)
        .position(position);
    frame.render_stateful_widget(scrollbar, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_scrollbar_when_nothing_to_scroll() {
        assert!(scrollbar_metrics(0, 0, 20).is_none());
    }

    #[test]
    fn thumb_reaches_both_ends() {
        let viewport = 20;
        // Live tail (offset 0, fully scrolled down): ratatui only parks the
        // thumb flush at the bottom when position == content_length - 1, so
        // this is exactly the invariant that fixes the "phantom room below".
        let (content, pos, vp) = scrollbar_metrics(50, 0, viewport).unwrap();
        assert_eq!(
            pos,
            content - 1,
            "at the live tail the thumb must be flush bottom"
        );
        assert_eq!(vp, viewport);

        // Oldest line (offset == scrollable_rows, fully scrolled up) → top.
        let (_content, pos_top, _vp) = scrollbar_metrics(50, 50, viewport).unwrap();
        assert_eq!(
            pos_top, 0,
            "scrolled all the way up the thumb must be at the top"
        );

        // Halfway back.
        let (_c, pos_mid, _v) = scrollbar_metrics(50, 25, viewport).unwrap();
        assert_eq!(pos_mid, 25);
    }
}

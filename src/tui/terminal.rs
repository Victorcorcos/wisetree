//! Terminal lifecycle: raw mode, fixed viewport, and panic-safe
//! restoration.
//!
//! The TUI renders into a fixed-height viewport anchored at the top of a
//! cleared terminal instead of taking over the alternate screen. This keeps
//! the compact stacked layout while avoiding the leading blank rows produced
//! by Ratatui's inline viewport initialization.
//!
//! Two flavors:
//! - `enter` renders to `stdout` (default).
//! - `enter_wrapper` renders to the controlling TTY (`/dev/tty` on Unix,
//!   `CONOUT$` on Windows). Used in `--from-wrapper` mode where real stdout
//!   is a pipe back to the shell — emitting any TUI bytes there would
//!   corrupt the `local dir=$(...)` capture.

use std::fs::{File, OpenOptions};
use std::io::{self, Stdout, Write};

use crossterm::style::available_color_count;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::{Backend as RatatuiBackend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::Color;
use ratatui::{Terminal as RatTerminal, TerminalOptions, Viewport};

/// Maximum number of rows the TUI reserves at the top of the terminal. Sized to fit
/// the tallest screen (Setup → Confirm preview); shorter screens leave the
/// trailing rows blank rather than painting them with a backdrop, so the
/// user's terminal background shows through.
pub const INLINE_VIEWPORT_HEIGHT: u16 = 25;

pub type Backend = AdaptiveBackend<Stdout>;
pub type WrapperBackend = AdaptiveBackend<File>;
pub type Terminal = RatTerminal<Backend>;
pub type WrapperTerminal = RatTerminal<WrapperBackend>;

/// Crossterm emits truecolor as `38;2;R;G;B` / `48;2;R;G;B`. On terminals
/// that only advertise `xterm-256color`, those parameters are sometimes
/// parsed as standalone SGR codes (`42` => green background, `46` => cyan,
/// etc.), which matches the broken neon colors seen in Terminal.app.
///
/// This wrapper downgrades `Color::Rgb` cells to 256-color or ANSI colors
/// before they reach Crossterm unless the environment explicitly reports
/// truecolor support.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct AdaptiveBackend<W: Write> {
    inner: CrosstermBackend<W>,
    color_count: u16,
}

impl<W: Write> AdaptiveBackend<W> {
    pub fn new(writer: W) -> Self {
        Self::with_color_count(writer, detect_color_count())
    }

    fn with_color_count(writer: W, color_count: u16) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            color_count,
        }
    }
}

/// Crossterm's `available_color_count()` reads `terminfo`'s `colors` capability,
/// which caps at 256 even on terminals (iTerm2, Terminal.app w/ truecolor
/// patch, Ghostty, WezTerm, modern xterm) that actually support 24-bit color.
/// Honor the de facto `COLORTERM` env var so those terminals get true RGB
/// instead of being forced through the 256-color cube.
fn detect_color_count() -> u16 {
    if let Ok(value) = std::env::var("COLORTERM") {
        let v = value.to_ascii_lowercase();
        if v == "truecolor" || v == "24bit" {
            return u16::MAX;
        }
    }
    available_color_count()
}

impl<W: Write> Write for AdaptiveBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

impl<W: Write> RatatuiBackend for AdaptiveBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        if self.color_count == u16::MAX {
            return self.inner.draw(content);
        }

        let downgraded: Vec<(u16, u16, Cell)> = content
            .map(|(x, y, cell)| {
                let mut cell = cell.clone();
                cell.fg = downgrade_color(cell.fg, self.color_count);
                cell.bg = downgrade_color(cell.bg, self.color_count);
                (x, y, cell)
            })
            .collect();

        self.inner
            .draw(downgraded.iter().map(|(x, y, cell)| (*x, *y, cell)))
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        RatatuiBackend::flush(&mut self.inner)
    }
}

fn downgrade_color(color: Color, color_count: u16) -> Color {
    match color {
        Color::Rgb(r, g, b) if color_count >= 256 => Color::Indexed(rgb_to_ansi256(r, g, b)),
        Color::Rgb(r, g, b) => rgb_to_ansi16(r, g, b),
        Color::Indexed(idx) if color_count < 256 => {
            let (r, g, b) = ansi256_to_rgb(idx);
            rgb_to_ansi16(r, g, b)
        }
        other => other,
    }
}

fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let to_cube = |value: u8| -> u8 {
        match value {
            0..=47 => 0,
            48..=114 => 1,
            _ => ((value as u16 - 35) / 40) as u8,
        }
    };
    let cube_levels = [0_u8, 95, 135, 175, 215, 255];

    let cr = to_cube(r).min(5);
    let cg = to_cube(g).min(5);
    let cb = to_cube(b).min(5);
    let cube_index = 16 + 36 * cr + 6 * cg + cb;
    let cube_rgb = (
        cube_levels[cr as usize],
        cube_levels[cg as usize],
        cube_levels[cb as usize],
    );

    let average = (r as u16 + g as u16 + b as u16) / 3;
    let gray_index = if average > 238 {
        23
    } else {
        ((average.saturating_sub(3)) / 10) as u8
    };
    let gray_level = 8 + gray_index * 10;
    let gray_rgb = (gray_level, gray_level, gray_level);
    let gray_index = 232 + gray_index;

    if color_distance((r, g, b), gray_rgb) < color_distance((r, g, b), cube_rgb) {
        gray_index
    } else {
        cube_index
    }
}

fn ansi256_to_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        0 => (0x00, 0x00, 0x00),
        1 => (0x80, 0x00, 0x00),
        2 => (0x00, 0x80, 0x00),
        3 => (0x80, 0x80, 0x00),
        4 => (0x00, 0x00, 0x80),
        5 => (0x80, 0x00, 0x80),
        6 => (0x00, 0x80, 0x80),
        7 => (0xc0, 0xc0, 0xc0),
        8 => (0x80, 0x80, 0x80),
        9 => (0xff, 0x00, 0x00),
        10 => (0x00, 0xff, 0x00),
        11 => (0xff, 0xff, 0x00),
        12 => (0x00, 0x00, 0xff),
        13 => (0xff, 0x00, 0xff),
        14 => (0x00, 0xff, 0xff),
        15 => (0xff, 0xff, 0xff),
        16..=231 => {
            let value = index - 16;
            let r = value / 36;
            let g = (value % 36) / 6;
            let b = value % 6;
            let level = |component: u8| -> u8 {
                if component == 0 {
                    0
                } else {
                    55 + component * 40
                }
            };
            (level(r), level(g), level(b))
        }
        232..=255 => {
            let level = 8 + (index - 232) * 10;
            (level, level, level)
        }
    }
}

fn rgb_to_ansi16(r: u8, g: u8, b: u8) -> Color {
    const ANSI16: &[(Color, (u8, u8, u8))] = &[
        (Color::Black, (0x00, 0x00, 0x00)),
        (Color::Red, (0x80, 0x00, 0x00)),
        (Color::Green, (0x00, 0x80, 0x00)),
        (Color::Yellow, (0x80, 0x80, 0x00)),
        (Color::Blue, (0x00, 0x00, 0x80)),
        (Color::Magenta, (0x80, 0x00, 0x80)),
        (Color::Cyan, (0x00, 0x80, 0x80)),
        (Color::Gray, (0xc0, 0xc0, 0xc0)),
        (Color::DarkGray, (0x80, 0x80, 0x80)),
        (Color::LightRed, (0xff, 0x00, 0x00)),
        (Color::LightGreen, (0x00, 0xff, 0x00)),
        (Color::LightYellow, (0xff, 0xff, 0x00)),
        (Color::LightBlue, (0x00, 0x00, 0xff)),
        (Color::LightMagenta, (0xff, 0x00, 0xff)),
        (Color::LightCyan, (0x00, 0xff, 0xff)),
        (Color::White, (0xff, 0xff, 0xff)),
    ];

    ANSI16
        .iter()
        .min_by_key(|(_, candidate)| color_distance((r, g, b), *candidate))
        .map(|(color, _)| *color)
        .unwrap_or(Color::White)
}

fn color_distance((r1, g1, b1): (u8, u8, u8), (r2, g2, b2): (u8, u8, u8)) -> u32 {
    let dr = r1 as i32 - r2 as i32;
    let dg = g1 as i32 - g2 as i32;
    let db = b1 as i32 - b2 as i32;
    (dr * dr + dg * dg + db * db) as u32
}

#[cfg(unix)]
const TTY_PATH: &str = "/dev/tty";
#[cfg(windows)]
const TTY_PATH: &str = "CONOUT$";

/// Install a panic hook that restores the terminal before delegating to the
/// previous hook. Idempotent — calling twice replaces the hook with an
/// equivalent one (the new hook chains through the previous, which itself
/// chains through the original).
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        let _ = restore_wrapper_tty();
        prev(info);
    }));
}

/// Set raw mode and return a top-anchored ratatui terminal handle.
pub fn enter() -> io::Result<Terminal> {
    enable_raw_mode()?;
    let backend = AdaptiveBackend::new(io::stdout());
    let size = backend.size()?;
    let viewport = app_viewport(size);
    let mut terminal = RatTerminal::with_options(backend, TerminalOptions { viewport })?;
    clear_terminal_for_app(&mut terminal)?;
    Ok(terminal)
}

/// Wrapper-mode entry: render to `/dev/tty` (Unix) / `CONOUT$` (Windows)
/// instead of stdout, so the parent shell's `$(...)` capture stays clean.
pub fn enter_wrapper() -> io::Result<WrapperTerminal> {
    enable_raw_mode()?;
    let tty = OpenOptions::new().read(true).write(true).open(TTY_PATH)?;
    let backend = AdaptiveBackend::new(tty);
    let size = backend.size()?;
    let viewport = app_viewport(size);
    let mut terminal = RatTerminal::with_options(backend, TerminalOptions { viewport })?;
    clear_terminal_for_app(&mut terminal)?;
    Ok(terminal)
}

fn app_viewport(size: Size) -> Viewport {
    let viewport_height = INLINE_VIEWPORT_HEIGHT.min(size.height);
    Viewport::Fixed(Rect::new(0, 0, size.width, viewport_height))
}

fn clear_terminal_for_app<B: RatatuiBackend>(terminal: &mut RatTerminal<B>) -> io::Result<()> {
    let backend = terminal.backend_mut();
    backend.clear_region(ClearType::All)?;
    backend.set_cursor_position(Position::ORIGIN)?;
    RatatuiBackend::flush(backend)
}

/// Clear the visible terminal and reset the cursor before returning control
/// to the parent shell. Wrapper mode can't preserve the original inline
/// cursor position reliably after switching away from Ratatui's inline
/// viewport, so we prefer a clean prompt at the top-left.
pub fn clear_wrapper_for_shell(terminal: &mut WrapperTerminal) -> io::Result<()> {
    clear_terminal_for_app(terminal)
}

/// Best-effort cleanup. Safe to call even if `enter` was never invoked.
pub fn restore() -> io::Result<()> {
    let _ = disable_raw_mode();
    let mut backend = AdaptiveBackend::new(io::stdout());
    let _ = backend.clear_region(ClearType::All);
    let _ = backend.set_cursor_position(Position::ORIGIN);
    let _ = RatatuiBackend::flush(&mut backend);
    Ok(())
}

/// Best-effort cleanup for wrapper mode.
pub fn restore_wrapper_tty() -> io::Result<()> {
    let _ = disable_raw_mode();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ratatui::backend::Backend as _;

    use super::*;

    #[derive(Clone, Default, Debug)]
    struct SharedWriter(Rc<RefCell<Vec<u8>>>);

    impl SharedWriter {
        fn new() -> Self {
            Self::default()
        }

        fn contents(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).unwrap()
        }
    }

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn styled_cell() -> Cell {
        let mut cell = Cell::default();
        cell.set_symbol("A");
        cell.set_fg(Color::Rgb(0xd4, 0xea, 0x9a));
        cell.set_bg(Color::Rgb(0x12, 0x2c, 0x38));
        cell
    }

    #[test]
    fn truecolor_backend_preserves_rgb_sequences() {
        let writer = SharedWriter::new();
        let mut backend = AdaptiveBackend::with_color_count(writer.clone(), u16::MAX);
        let cell = styled_cell();

        backend.draw(std::iter::once((0, 0, &cell))).unwrap();

        let output = writer.contents();
        assert!(output.contains("38;2;212;234;154"));
        assert!(output.contains("48;2;18;44;56"));
    }

    #[test]
    fn ansi256_backend_avoids_truecolor_sequences() {
        let writer = SharedWriter::new();
        let mut backend = AdaptiveBackend::with_color_count(writer.clone(), 256);
        let cell = styled_cell();

        backend.draw(std::iter::once((0, 0, &cell))).unwrap();

        let output = writer.contents();
        assert!(!output.contains(";2;"));
        assert!(output.contains("38;5;"));
        assert!(output.contains("48;5;"));
    }

    #[test]
    fn app_viewport_is_top_anchored_and_capped() {
        assert_eq!(
            app_viewport(Size::new(80, 40)),
            Viewport::Fixed(Rect::new(0, 0, 80, 25))
        );
        assert_eq!(
            app_viewport(Size::new(80, 20)),
            Viewport::Fixed(Rect::new(0, 0, 80, 20))
        );
    }
}

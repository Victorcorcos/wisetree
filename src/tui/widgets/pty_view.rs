//! Embedded PTY view that hosts a subprocess (opencode) inside a ratatui
//! panel. Spawns the command on a real PTY via `portable-pty`, pipes its
//! raw output through a `vt100::Parser`, and blits the resulting screen
//! cells into the panel area on each frame so the subprocess sees a
//! genuine terminal (and its TUI / ANSI styling renders the way it would
//! standalone).
//!
//! The reader runs on an OS thread (portable-pty's reader is `Read`, not
//! `AsyncRead`); the main UI thread polls `tick()` each frame to check
//! whether the child has exited. When the user navigates away or the
//! screen tears down, `Drop` kills the child and joins the reader thread.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use ratatui::buffer::Cell as BufferCell;
use ratatui::layout::Rect;
use ratatui::style::{Color as RatColor, Modifier, Style};
use ratatui::Frame;
use vt100::{Color as VtColor, Parser};

/// Initial PTY dimensions used before the first render-driven resize.
/// vt100 happily handles resizes once we know the panel area, so this
/// only matters for the very first batch of output before the first
/// frame paints. The opencode CLI flushes a small banner that we'd
/// rather not have wrap awkwardly, so we start generous.
const DEFAULT_ROWS: u16 = 40;
const DEFAULT_COLS: u16 = 160;
/// How many lines of scrollback the vt100 parser retains. opencode runs
/// can emit thousands of formatted lines (Thinking blocks, diffs, tool
/// output); 5000 rows gives the user plenty of history to scroll back
/// through without ballooning memory.
const SCROLLBACK_ROWS: usize = 5000;

pub struct PtyView {
    parser: Arc<Mutex<Parser>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    reader_handle: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
    exit_status: Arc<Mutex<Option<i32>>>,
    /// Last (rows, cols) the parser/PTY were resized to. Avoids a
    /// resize ioctl on every frame when the area hasn't actually changed.
    last_size: (u16, u16),
}

impl PtyView {
    pub fn spawn(
        binary: &Path,
        args: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
    ) -> std::io::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: DEFAULT_ROWS,
                cols: DEFAULT_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| std::io::Error::other(format!("openpty: {err}")))?;

        let mut cmd = CommandBuilder::new(binary);
        for arg in args {
            cmd.arg(arg);
        }
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        // opencode (and most CLIs) check $TERM to decide whether to emit
        // ANSI; force a sensible default so the embed always lights up.
        if !env.iter().any(|(k, _)| k == "TERM") {
            cmd.env("TERM", "xterm-256color");
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| std::io::Error::other(format!("spawn: {err}")))?;
        // The slave handle keeps the child's controlling tty open. We
        // drop it after spawn — the child holds its own dup'd fds for
        // stdin/stdout/stderr and the master keeps the kernel pty pair
        // alive.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| std::io::Error::other(format!("clone reader: {err}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|err| std::io::Error::other(format!("take writer: {err}")))?;

        let parser = Arc::new(Mutex::new(Parser::new(
            DEFAULT_ROWS,
            DEFAULT_COLS,
            SCROLLBACK_ROWS,
        )));
        let done = Arc::new(AtomicBool::new(false));
        let exit_status: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));

        let reader_parser = Arc::clone(&parser);
        let reader_done = Arc::clone(&done);
        let reader_handle = thread::Builder::new()
            .name("wisetree-pty-reader".into())
            .spawn(move || {
                let mut reader = reader;
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(mut parser) = reader_parser.lock() {
                                parser.process(&buf[..n]);
                            }
                        }
                        Err(err) => {
                            // EIO is normal when the slave side closes;
                            // anything else we treat as terminal too.
                            let _ = err;
                            break;
                        }
                    }
                }
                reader_done.store(true, Ordering::SeqCst);
            })
            .ok();

        Ok(Self {
            parser,
            master: pair.master,
            writer,
            child: Arc::new(Mutex::new(child)),
            reader_handle,
            done,
            exit_status,
            last_size: (DEFAULT_ROWS, DEFAULT_COLS),
        })
    }

    /// Returns true on the first call after the child has exited. The
    /// caller uses this edge to flip the screen into the Complete/Cancel
    /// step.
    pub fn poll_exited(&mut self) -> bool {
        if self.done.load(Ordering::SeqCst) {
            // Reader thread saw EOF — wait the child to harvest the
            // status (try_wait is fine; reader EOF means the child has
            // either exited or is about to).
            if let Ok(mut child) = self.child.lock() {
                if let Ok(Some(status)) = child.try_wait() {
                    let code = status.exit_code() as i32;
                    if let Ok(mut slot) = self.exit_status.lock() {
                        *slot = Some(code);
                    }
                    return true;
                }
                // try_wait says still running; trust the done flag and
                // kill to avoid a stuck child holding the screen open.
                let _ = child.kill();
                let _ = child.wait();
                return true;
            }
            return true;
        }
        false
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == 0 || cols == 0 {
            return;
        }
        if (rows, cols) == self.last_size {
            return;
        }
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut parser) = self.parser.lock() {
            parser.set_size(rows, cols);
        }
        self.last_size = (rows, cols);
    }

    pub fn send_input(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Total scrollback rows currently retained by the vt100 parser.
    /// vt100's `Screen` only exposes the *current* scrollback offset, not
    /// the buffer length — so we probe by saving the offset, asking
    /// `set_scrollback` to clamp at the maximum (it caps at the actual
    /// length), reading it back, then restoring. The whole operation
    /// runs under the parser mutex so the reader thread can't race.
    pub fn scrollback_len(&self) -> usize {
        if let Ok(mut parser) = self.parser.lock() {
            let saved = parser.screen().scrollback();
            parser.set_scrollback(usize::MAX);
            let len = parser.screen().scrollback();
            parser.set_scrollback(saved);
            return len;
        }
        0
    }

    /// Current scrollback offset — `0` means we're at the bottom (live
    /// tail), larger values mean we've moved further back in history.
    pub fn scrollback_offset(&self) -> usize {
        self.parser
            .lock()
            .map(|p| p.screen().scrollback())
            .unwrap_or(0)
    }

    /// Scroll the view back by `lines` rows. Clamped to scrollback_len
    /// by vt100, so over-scroll is safe.
    pub fn scroll_up(&mut self, lines: u16) {
        if let Ok(mut parser) = self.parser.lock() {
            let target = parser.screen().scrollback().saturating_add(lines as usize);
            parser.set_scrollback(target);
        }
    }

    /// Scroll the view forward by `lines` rows toward the live tail.
    pub fn scroll_down(&mut self, lines: u16) {
        if let Ok(mut parser) = self.parser.lock() {
            let target = parser
                .screen()
                .scrollback()
                .saturating_sub(lines as usize);
            parser.set_scrollback(target);
        }
    }

    /// Snap to the top of the scrollback (oldest line at top of view).
    pub fn scroll_to_top(&mut self) {
        if let Ok(mut parser) = self.parser.lock() {
            // `set_scrollback` clamps internally — passing usize::MAX
            // lands us at the actual top regardless of buffer size.
            parser.set_scrollback(usize::MAX);
        }
    }

    /// Snap back to the live tail.
    pub fn scroll_to_bottom(&mut self) {
        if let Ok(mut parser) = self.parser.lock() {
            parser.set_scrollback(0);
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let parser = match self.parser.lock() {
            Ok(p) => p,
            Err(_) => return,
        };
        let screen = parser.screen();
        let buf = frame.buffer_mut();
        for row in 0..area.height {
            for col in 0..area.width {
                let Some(vt_cell) = screen.cell(row, col) else {
                    continue;
                };
                let dest_x = area.x + col;
                let dest_y = area.y + row;
                let dest: &mut BufferCell = &mut buf[(dest_x, dest_y)];
                let contents = vt_cell.contents();
                if contents.is_empty() {
                    dest.set_symbol(" ");
                } else {
                    dest.set_symbol(&contents);
                }
                let mut style = Style::default()
                    .fg(convert_color(vt_cell.fgcolor(), true))
                    .bg(convert_color(vt_cell.bgcolor(), false));
                let mut modifier = Modifier::empty();
                if vt_cell.bold() {
                    modifier |= Modifier::BOLD;
                }
                if vt_cell.italic() {
                    modifier |= Modifier::ITALIC;
                }
                if vt_cell.underline() {
                    modifier |= Modifier::UNDERLINED;
                }
                if vt_cell.inverse() {
                    modifier |= Modifier::REVERSED;
                }
                style = style.add_modifier(modifier);
                dest.set_style(style);
            }
        }
    }
}

impl Drop for PtyView {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
    }
}

fn convert_color(c: VtColor, _is_fg: bool) -> RatColor {
    match c {
        VtColor::Default => RatColor::Reset,
        VtColor::Idx(i) => match i {
            0 => RatColor::Black,
            1 => RatColor::Red,
            2 => RatColor::Green,
            3 => RatColor::Yellow,
            4 => RatColor::Blue,
            5 => RatColor::Magenta,
            6 => RatColor::Cyan,
            7 => RatColor::Gray,
            8 => RatColor::DarkGray,
            9 => RatColor::LightRed,
            10 => RatColor::LightGreen,
            11 => RatColor::LightYellow,
            12 => RatColor::LightBlue,
            13 => RatColor::LightMagenta,
            14 => RatColor::LightCyan,
            15 => RatColor::White,
            other => RatColor::Indexed(other),
        },
        VtColor::Rgb(r, g, b) => RatColor::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn spawn_echo() -> PtyView {
        PtyView::spawn(Path::new("/bin/echo"), &["hello".to_string()], None, &[])
            .expect("spawn /bin/echo")
    }

    fn wait_for_exit(pty: &mut PtyView) {
        let deadline = Instant::now() + Duration::from_millis(2000);
        while Instant::now() < deadline {
            if pty.poll_exited() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn scroll_apis_clamp_safely_and_dont_panic_on_overscroll() {
        let mut pty = spawn_echo();
        wait_for_exit(&mut pty);

        // Over-scroll past the available scrollback (likely zero rows for
        // a one-line `echo`): all of these must no-op without panicking.
        pty.scroll_up(u16::MAX);
        pty.scroll_down(u16::MAX);
        pty.scroll_up(50);
        pty.scroll_down(50);
        pty.scroll_to_top();
        pty.scroll_to_bottom();
        assert_eq!(pty.scrollback_offset(), 0);

        // scrollback_len matches what vt100 actually retains; for a
        // single-line echo this is 0, but the accessor must still work.
        let _ = pty.scrollback_len();
    }
}

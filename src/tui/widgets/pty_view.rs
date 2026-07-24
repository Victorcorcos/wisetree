//! Embedded PTY view that hosts an AI CLI subprocess inside a ratatui
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

use std::cell::Cell;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use ratatui::buffer::Cell as BufferCell;
use ratatui::layout::Rect;
use ratatui::style::{Color as RatColor, Modifier, Style};
use ratatui::Frame;
use vt100::{Color as VtColor, MouseProtocolEncoding, MouseProtocolMode, Parser};

/// Initial PTY dimensions used before the first render-driven resize.
/// vt100 happily handles resizes once we know the panel area, so this
/// only matters for the very first batch of output before the first
/// frame paints. The opencode CLI flushes a small banner that we'd
/// rather not have wrap awkwardly, so we start generous.
const DEFAULT_ROWS: u16 = 40;
const DEFAULT_COLS: u16 = 160;
/// How many lines of scrollback the vt100 parser retains. AI CLI runs
/// can emit thousands of formatted lines (Thinking blocks, diffs, tool
/// output); 5000 rows gives the user plenty of history to scroll back
/// through without ballooning memory.
const SCROLLBACK_ROWS: usize = 5000;
/// `PageUp` / `PageDown` key reports, sent to alt-screen children that own
/// their own scroll region (see [`PtyView::wheel_up`]).
const PAGE_UP: &[u8] = b"\x1b[5~";
const PAGE_DOWN: &[u8] = b"\x1b[6~";

/// Reply to an `OSC 10` (query default foreground) request: a light gray,
/// matching a dark terminal theme. Terminated with ST (`ESC \`).
const OSC10_FG_REPLY: &[u8] = b"\x1b]10;rgb:c7c7/c7c7/c7c7\x1b\\";
/// Reply to an `OSC 11` (query default background) request: a dark charcoal.
///
/// Codex (unlike claude) refuses to paint its themed message blocks until it
/// learns the terminal background this way — with no reply it downgrades to a
/// flat, block-less theme. Reporting a dark background unlocks the same
/// rendering it shows in a standalone dark terminal. (A light-terminal user
/// would want a light value here; the reply is assembled in
/// [`terminal_query_reply`]. A future improvement could relay the host
/// terminal's actual background instead of this fixed dark default.)
const OSC11_BG_REPLY: &[u8] = b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\";

pub struct PtyView {
    parser: Arc<Mutex<Parser>>,
    master: Box<dyn MasterPty + Send>,
    /// Shared with the reader thread so it can answer terminal capability
    /// queries (OSC 10/11) the instant the child asks, exactly like a real
    /// terminal — the master exposes only a single writer, so both the host
    /// (`send_input`) and the reader thread go through this one handle.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    reader_handle: Option<JoinHandle<()>>,
    done: Arc<AtomicBool>,
    exit_status: Arc<Mutex<Option<i32>>>,
    /// Last (rows, cols) the parser/PTY were resized to. Avoids a
    /// resize ioctl on every frame when the area hasn't actually changed.
    last_size: (u16, u16),
    /// The panel `Rect` the PTY was last rendered into. Used to translate
    /// absolute host mouse coordinates into the child's cell grid when
    /// forwarding mouse reports.
    last_area: Cell<Rect>,
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
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pair.master.take_writer().map_err(|err| {
                std::io::Error::other(format!("take writer: {err}"))
            })?));

        let parser = Arc::new(Mutex::new(Parser::new(
            DEFAULT_ROWS,
            DEFAULT_COLS,
            SCROLLBACK_ROWS,
        )));
        let done = Arc::new(AtomicBool::new(false));
        let exit_status: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));

        let reader_parser = Arc::clone(&parser);
        let reader_done = Arc::clone(&done);
        let reader_writer = Arc::clone(&writer);
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
                            // Answer terminal capability queries the way a real
                            // terminal would. Codex refuses to paint its themed
                            // blocks until it learns the background via OSC 11.
                            if let Some(reply) = terminal_query_reply(&buf[..n]) {
                                if let Ok(mut writer) = reader_writer.lock() {
                                    let _ = writer.write_all(&reply);
                                    let _ = writer.flush();
                                }
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
            last_area: Cell::new(Rect::default()),
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
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    /// Forward a host mouse event to the child, encoded per the mouse
    /// protocol the child has enabled (opencode turns on SGR any-motion
    /// tracking through opentui). `abs_col` / `abs_row` are absolute host
    /// terminal coordinates; they're translated into the child's cell grid
    /// using the last render area.
    ///
    /// Returns `true` when the child is tracking the mouse and the event was
    /// handed off — the host then treats the event as consumed instead of
    /// running its own text-selection / scrollback handling. Returns `false`
    /// when the child has no mouse mode active, so the host keeps its native
    /// wheel + selection behavior (e.g. a plain recovery shell that never
    /// enabled mouse reporting). Also returns `false` for wheel events aimed
    /// at a non-alt-screen child (codex, claude) even if it happens to have a
    /// mouse mode active, since those harnesses scroll through committed
    /// history rather than a mouse-driven scroll region and don't understand
    /// wheel reports — the caller's `wheel_up`/`wheel_down` fallback handles
    /// scrolling for them instead.
    pub fn send_mouse(
        &mut self,
        kind: MouseEventKind,
        abs_col: u16,
        abs_row: u16,
        modifiers: KeyModifiers,
    ) -> bool {
        let (mode, encoding, alternate_screen) = match self.parser.lock() {
            Ok(parser) => {
                let screen = parser.screen();
                (
                    screen.mouse_protocol_mode(),
                    screen.mouse_protocol_encoding(),
                    screen.alternate_screen(),
                )
            }
            Err(_) => return false,
        };
        if mode == MouseProtocolMode::None {
            return false;
        }
        // Inline harnesses (codex, claude) render without an alt screen and
        // scroll through committed history rather than a mouse-driven scroll
        // region. Some of them still enable a mouse-tracking mode (e.g. to
        // support click-to-position in their composer) without understanding
        // wheel reports — forwarding one raw gets echoed back as literal text
        // and can even read as an interrupt keystroke. Let the caller's
        // PageUp/PageDown-or-local-scrollback fallback (`wheel_up`/
        // `wheel_down`) handle wheel events for these instead.
        if !alternate_screen
            && matches!(
                kind,
                MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight
            )
        {
            return false;
        }
        let area = self.last_area.get();
        if area.width == 0 || area.height == 0 {
            return false;
        }
        // Clamp into the rendered grid so an event that strays outside the
        // panel (a drag past its edge) still reports a valid edge cell.
        let col = abs_col.saturating_sub(area.x).min(area.width - 1);
        let row = abs_row.saturating_sub(area.y).min(area.height - 1);
        if let Some(bytes) = encode_mouse_report(kind, col, row, modifiers, mode, encoding) {
            self.send_input(&bytes);
        }
        // The child owns the mouse whenever it is tracking — even for an event
        // this mode doesn't report (e.g. a bare move under button-motion mode),
        // the host must not also act on it.
        true
    }

    /// Whether the child has enabled any mouse-tracking mode. Alt-screen
    /// TUIs (opencode) turn tracking on and own the wheel — the host forwards
    /// wheel events to them as SGR reports. Inline-rendering harnesses (codex,
    /// claude) never enable tracking and instead scroll through the terminal's
    /// scrollback, so the host must drive the vt100 buffer itself.
    pub fn tracks_mouse(&self) -> bool {
        self.parser
            .lock()
            .map(|p| p.screen().mouse_protocol_mode() != MouseProtocolMode::None)
            .unwrap_or(false)
    }

    /// One wheel tick / scroll key toward older output. Alt-screen TUIs that
    /// track the mouse (opencode) manage their own scroll region, so we send
    /// them a `PageUp` key; inline harnesses (codex, claude) never enter the
    /// alt screen and scroll by moving through the vt100 scrollback buffer.
    pub fn wheel_up(&mut self, lines: u16) {
        if self.tracks_mouse() {
            self.send_input(PAGE_UP);
        } else {
            self.scroll_up(lines);
        }
    }

    /// One wheel tick / scroll key toward the live tail. See [`wheel_up`].
    ///
    /// [`wheel_up`]: Self::wheel_up
    pub fn wheel_down(&mut self, lines: u16) {
        if self.tracks_mouse() {
            self.send_input(PAGE_DOWN);
        } else {
            self.scroll_down(lines);
        }
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
            let target = parser.screen().scrollback().saturating_sub(lines as usize);
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

    /// Returns the exit code recorded when the child exited. `None` when the
    /// child is still running or the kill/wait path was taken (status
    /// unavailable). Only meaningful after `poll_exited` has returned `true`.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_status.lock().ok().and_then(|slot| *slot)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.last_area.set(area);
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
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    _ => break,
                }
            }
        }
        if let Some(handle) = self.reader_handle.take() {
            let deadline = Instant::now() + Duration::from_millis(500);
            // Drop the handle if the reader thread hasn't finished by the deadline.
            // The process is exiting anyway so the thread will be cleaned up by the OS.
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

/// Build the reply a real terminal would send for any capability query found
/// in `chunk`, or `None` if there are none. We answer only the two queries an
/// inline harness needs to theme itself — `OSC 10` (default foreground) and
/// `OSC 11` (default background). We deliberately do **not** answer device
/// attributes / cursor-position / kitty-keyboard probes: they're unnecessary
/// for styling and a wrong answer risks changing input handling.
///
/// The query byte strings are tiny and codex emits them together in its opening
/// burst, so a per-read scan (rather than a reassembling buffer) reliably
/// catches them; a query split across two reads would simply go unanswered.
fn terminal_query_reply(chunk: &[u8]) -> Option<Vec<u8>> {
    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
    let mut reply = Vec::new();
    if contains(chunk, b"\x1b]10;?") {
        reply.extend_from_slice(OSC10_FG_REPLY);
    }
    if contains(chunk, b"\x1b]11;?") {
        reply.extend_from_slice(OSC11_BG_REPLY);
    }
    (!reply.is_empty()).then_some(reply)
}

/// Encode a mouse event as an xterm mouse report for the child terminal.
/// `col` / `row` are 0-based cell coordinates. Returns `None` when the active
/// `mode` doesn't report this event kind (e.g. a bare move while the child
/// only asked for button-motion tracking), so the caller writes nothing.
fn encode_mouse_report(
    kind: MouseEventKind,
    col: u16,
    row: u16,
    modifiers: KeyModifiers,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    // Low two bits select the button (0 left, 1 middle, 2 right, 3 none);
    // bit 5 (0x20) flags motion; bit 6 (0x40) flags a wheel button.
    const MOTION: u8 = 0x20;
    let (mut cb, is_release, reportable) = match kind {
        MouseEventKind::Down(button) => (button_code(button), false, true),
        // X10 press-only mode never reports releases.
        MouseEventKind::Up(button) => (button_code(button), true, mode != MouseProtocolMode::Press),
        MouseEventKind::Drag(button) => (
            button_code(button) | MOTION,
            false,
            matches!(
                mode,
                MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
            ),
        ),
        MouseEventKind::Moved => (0x03 | MOTION, false, mode == MouseProtocolMode::AnyMotion),
        MouseEventKind::ScrollUp => (0x40, false, true),
        MouseEventKind::ScrollDown => (0x41, false, true),
        MouseEventKind::ScrollLeft => (0x42, false, true),
        MouseEventKind::ScrollRight => (0x43, false, true),
    };
    if !reportable {
        return None;
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        cb |= 0x04;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        cb |= 0x08;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        cb |= 0x10;
    }

    match encoding {
        MouseProtocolEncoding::Sgr => {
            let final_byte = if is_release { 'm' } else { 'M' };
            Some(
                format!(
                    "\x1b[<{};{};{}{}",
                    cb,
                    u32::from(col) + 1,
                    u32::from(row) + 1,
                    final_byte
                )
                .into_bytes(),
            )
        }
        // Legacy single-byte encoding (Default) and its UTF-8 variant, which
        // only diverge for coordinates past 95 — rare inside a panel. A
        // release sets the button bits to 3.
        MouseProtocolEncoding::Default | MouseProtocolEncoding::Utf8 => {
            let cb = if is_release { (cb & !0x03) | 0x03 } else { cb };
            // Each field is offset by 32; coordinates are 1-based and clamp at
            // the 223-cell ceiling the single-byte form can express.
            let coord = |v: u16| -> u8 { (v.min(222) as u8) + 1 + 32 };
            Some(vec![0x1b, b'[', b'M', cb + 32, coord(col), coord(row)])
        }
    }
}

/// The xterm button code for a mouse button (before motion / modifier bits).
fn button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
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
    use std::ffi::OsString;
    use std::time::{Duration, Instant};

    fn resolve_on_path(binary: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).find_map(|dir| {
            let candidate = dir.join(binary);
            candidate.is_file().then_some(candidate)
        })
    }

    fn echo_binary() -> std::path::PathBuf {
        resolve_on_path("echo")
            .or_else(|| {
                ["/bin/echo", "/usr/bin/echo"]
                    .into_iter()
                    .map(OsString::from)
                    .map(std::path::PathBuf::from)
                    .find(|path| path.is_file())
            })
            .expect("echo available on PATH")
    }

    fn spawn_echo() -> PtyView {
        PtyView::spawn(&echo_binary(), &["hello".to_string()], None, &[]).expect("spawn echo")
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

    fn sgr(bytes: Option<Vec<u8>>) -> String {
        String::from_utf8(bytes.expect("expected a mouse report")).unwrap()
    }

    #[test]
    fn sgr_encodes_press_release_and_motion_with_one_based_coords() {
        // Left press at cell (0,0) → button 0, `M` terminator, 1-based coords.
        assert_eq!(
            sgr(encode_mouse_report(
                MouseEventKind::Down(MouseButton::Left),
                0,
                0,
                KeyModifiers::NONE,
                MouseProtocolMode::AnyMotion,
                MouseProtocolEncoding::Sgr,
            )),
            "\x1b[<0;1;1M"
        );
        // Release uses the lowercase `m` terminator, keeping the button code.
        assert_eq!(
            sgr(encode_mouse_report(
                MouseEventKind::Up(MouseButton::Left),
                3,
                1,
                KeyModifiers::NONE,
                MouseProtocolMode::AnyMotion,
                MouseProtocolEncoding::Sgr,
            )),
            "\x1b[<0;4;2m"
        );
        // Bare move → no button + motion flag (0x03 | 0x20 = 35).
        assert_eq!(
            sgr(encode_mouse_report(
                MouseEventKind::Moved,
                4,
                2,
                KeyModifiers::NONE,
                MouseProtocolMode::AnyMotion,
                MouseProtocolEncoding::Sgr,
            )),
            "\x1b[<35;5;3M"
        );
        // Wheel-up carries button bit 6 (0x40 = 64).
        assert_eq!(
            sgr(encode_mouse_report(
                MouseEventKind::ScrollUp,
                0,
                0,
                KeyModifiers::NONE,
                MouseProtocolMode::AnyMotion,
                MouseProtocolEncoding::Sgr,
            )),
            "\x1b[<64;1;1M"
        );
    }

    #[test]
    fn mouse_reports_are_gated_by_the_childs_tracking_mode() {
        // A bare move is only reported under any-motion tracking.
        assert!(encode_mouse_report(
            MouseEventKind::Moved,
            1,
            1,
            KeyModifiers::NONE,
            MouseProtocolMode::ButtonMotion,
            MouseProtocolEncoding::Sgr,
        )
        .is_none());
        // A drag needs at least button-motion tracking.
        assert!(encode_mouse_report(
            MouseEventKind::Drag(MouseButton::Left),
            1,
            1,
            KeyModifiers::NONE,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Sgr,
        )
        .is_none());
        // X10 press-only mode never reports releases.
        assert!(encode_mouse_report(
            MouseEventKind::Up(MouseButton::Left),
            1,
            1,
            KeyModifiers::NONE,
            MouseProtocolMode::Press,
            MouseProtocolEncoding::Sgr,
        )
        .is_none());
    }

    #[test]
    fn default_encoding_offsets_every_field_by_32() {
        // Left press at (0,0): button 32, coords (1+32)=33 → matches the
        // classic single-byte `CSI M` report.
        assert_eq!(
            encode_mouse_report(
                MouseEventKind::Down(MouseButton::Left),
                0,
                0,
                KeyModifiers::NONE,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Default,
            ),
            Some(vec![0x1b, b'[', b'M', 32, 33, 33])
        );
        // Release collapses the button bits to 3 (32 + 3 = 35).
        assert_eq!(
            encode_mouse_report(
                MouseEventKind::Up(MouseButton::Left),
                0,
                0,
                KeyModifiers::NONE,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Default,
            ),
            Some(vec![0x1b, b'[', b'M', 35, 33, 33])
        );
    }

    #[test]
    fn send_mouse_is_a_no_op_when_the_child_isnt_tracking() {
        // A freshly spawned `echo` never enables mouse reporting, so the host
        // keeps ownership of the event (returns false).
        let mut pty = spawn_echo();
        assert!(!pty.send_mouse(
            MouseEventKind::Down(MouseButton::Left),
            0,
            0,
            KeyModifiers::NONE
        ));
    }

    #[test]
    fn send_mouse_forwards_once_the_child_enables_tracking() {
        // End-to-end proof of the mechanism: a real child that turns on
        // any-motion + SGR tracking (exactly what opencode's opentui does)
        // must flip `send_mouse` from a no-op into a forwarded report.
        let Some(sh) = resolve_on_path("sh") else {
            return; // no POSIX shell — skip rather than fail on odd hosts.
        };
        let mut pty = PtyView::spawn(
            &sh,
            &[
                "-c".to_string(),
                "printf '\\033[?1003h\\033[?1006h'; sleep 2".to_string(),
            ],
            None,
            &[],
        )
        .expect("spawn sh");
        // Pretend the panel was rendered so coordinate translation has an area.
        pty.last_area.set(Rect::new(0, 0, 80, 24));

        // Wait for the reader thread to feed the mode-enable escape into vt100.
        let deadline = Instant::now() + Duration::from_millis(2000);
        let mut enabled = false;
        while Instant::now() < deadline {
            if pty
                .parser
                .lock()
                .map(|p| p.screen().mouse_protocol_mode() != MouseProtocolMode::None)
                .unwrap_or(false)
            {
                enabled = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(enabled, "child never enabled mouse tracking");
        assert!(
            pty.send_mouse(
                MouseEventKind::Down(MouseButton::Left),
                0,
                0,
                KeyModifiers::NONE
            ),
            "a tracking child must consume the mouse event"
        );
    }

    #[test]
    fn send_mouse_never_forwards_raw_wheel_to_a_non_alt_screen_tracker() {
        // Reproduces the codex bug: an inline (non-alt-screen) harness that
        // enables a mouse mode (e.g. for click-to-position in its composer)
        // must NOT receive a raw SGR wheel report — codex doesn't decode
        // those, so they were echoed back as literal `[<64;...M` text and
        // even read as an interrupt keystroke. `send_mouse` must decline the
        // event (false) so the caller's wheel_up/wheel_down fallback runs.
        let Some(sh) = resolve_on_path("sh") else {
            return;
        };
        let mut pty = PtyView::spawn(
            &sh,
            &[
                "-c".to_string(),
                "printf '\\033[?1000h'; sleep 2".to_string(),
            ],
            None,
            &[],
        )
        .expect("spawn sh");
        pty.last_area.set(Rect::new(0, 0, 80, 24));

        let deadline = Instant::now() + Duration::from_millis(2000);
        let mut enabled = false;
        while Instant::now() < deadline {
            if pty
                .parser
                .lock()
                .map(|p| p.screen().mouse_protocol_mode() != MouseProtocolMode::None)
                .unwrap_or(false)
            {
                enabled = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(enabled, "child never enabled mouse tracking");
        assert!(
            pty.parser
                .lock()
                .map(|p| !p.screen().alternate_screen())
                .unwrap_or(false),
            "sh never enters the alternate screen"
        );
        assert!(
            !pty.send_mouse(MouseEventKind::ScrollUp, 0, 0, KeyModifiers::NONE),
            "wheel events must not be forwarded raw to a non-alt-screen tracker"
        );
        assert!(
            !pty.send_mouse(MouseEventKind::ScrollDown, 0, 0, KeyModifiers::NONE),
            "wheel events must not be forwarded raw to a non-alt-screen tracker"
        );
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

    #[test]
    fn wheel_scrolls_the_vt100_buffer_for_an_inline_child() {
        // codex and claude render inline and never enable mouse tracking, so a
        // wheel tick must move the vt100 scrollback — sending them a page key
        // (as we do for alt-screen children) would do nothing.
        let Some(sh) = resolve_on_path("sh") else {
            return; // no POSIX shell — skip rather than fail on odd hosts.
        };
        let mut pty = PtyView::spawn(
            &sh,
            &["-c".to_string(), "seq 1 200; sleep 2".to_string()],
            None,
            &[],
        )
        .expect("spawn sh");

        // Wait for enough output to build scrollback (200 lines > the grid).
        let deadline = Instant::now() + Duration::from_millis(2000);
        while Instant::now() < deadline && pty.scrollback_len() == 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(pty.scrollback_len() > 0, "child produced no scrollback");
        assert!(!pty.tracks_mouse());

        assert_eq!(pty.scrollback_offset(), 0);
        pty.wheel_up(5);
        assert_eq!(pty.scrollback_offset(), 5, "inline child must scroll vt100");
        pty.wheel_down(3);
        assert_eq!(pty.scrollback_offset(), 2);
    }

    /// Read the visible grid row `row` as a trimmed string (respects the
    /// current scrollback offset, since `cell` composites scrollback + screen).
    fn visible_row_text(pty: &PtyView, row: u16, cols: u16) -> String {
        let parser = pty.parser.lock().unwrap();
        let screen = parser.screen();
        let mut s = String::new();
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                s.push_str(&cell.contents());
            }
        }
        s.trim_end().to_string()
    }

    #[test]
    fn terminal_query_reply_answers_only_osc_10_and_11() {
        // No query -> nothing to send.
        assert!(terminal_query_reply(b"just some normal output\r\n").is_none());

        // OSC 10 + OSC 11 back-to-back (codex's opening burst) -> both replies,
        // foreground before background, each ST-terminated.
        let reply =
            terminal_query_reply(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?2004h").expect("reply");
        assert_eq!(reply, [OSC10_FG_REPLY, OSC11_BG_REPLY].concat());

        // Background query alone -> only the OSC 11 reply (the one that unlocks
        // codex's themed blocks).
        assert_eq!(
            terminal_query_reply(b"\x1b]11;?\x07").as_deref(),
            Some(OSC11_BG_REPLY)
        );

        // We must not answer device-attributes / cursor-position probes.
        assert!(terminal_query_reply(b"\x1b[c\x1b[6n").is_none());
    }

    #[test]
    fn spawned_child_gets_its_background_query_answered() {
        // End-to-end: a child that emits the OSC 11 query must receive our
        // reply on its stdin — the mechanism codex relies on to theme itself.
        let Some(sh) = resolve_on_path("sh") else {
            return;
        };
        // Emit the bg query, then echo whatever arrives on stdin so the test can
        // observe the reply the reader thread wrote back.
        let script = "printf '\\033]11;?\\033\\\\'; head -c 32";
        let pty = PtyView::spawn(&sh, &["-c".to_string(), script.to_string()], None, &[])
            .expect("spawn sh");

        let deadline = Instant::now() + Duration::from_millis(2000);
        let mut saw_reply = false;
        while Instant::now() < deadline {
            let text = {
                let parser = pty.parser.lock().unwrap();
                parser.screen().contents()
            };
            // The child echoes our reply; its distinctive payload is the rgb spec.
            if text.contains("rgb:1e1e") {
                saw_reply = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            saw_reply,
            "the reader thread must answer the child's OSC 11 background query"
        );
    }

    #[test]
    fn inline_child_scroll_region_history_reaches_scrollback() {
        // The crux of the codex/claude embed: they commit transcript lines by
        // scrolling *within a top-anchored DECSTBM scroll region* (reserving a
        // bottom composer), never entering the alt screen. Stock vt100 discards
        // rows scrolled out of an active region, so their history never reached
        // scrollback and the panel couldn't scroll back. The vendored patch
        // captures top-anchored regions — this test would see `scrollback_len`
        // stuck at 0 without it.
        let Some(sh) = resolve_on_path("sh") else {
            return; // no POSIX shell — skip rather than fail on odd hosts.
        };
        // Set a scroll region of rows 1..20 (top-anchored, bottom composer
        // reserved), park the cursor at its bottom, then emit 100 lines so the
        // region scrolls ~80 times. Mirrors how codex commits its transcript.
        let script = "printf '\\033[1;20r\\033[20;1H'; \
                      for i in $(seq 1 100); do printf 'line%d\\r\\n' \"$i\"; done; \
                      sleep 2";
        let mut pty = PtyView::spawn(&sh, &["-c".to_string(), script.to_string()], None, &[])
            .expect("spawn sh");

        let deadline = Instant::now() + Duration::from_millis(2000);
        while Instant::now() < deadline && pty.scrollback_len() == 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            pty.scrollback_len() > 0,
            "top-anchored scroll-region history must land in scrollback (patch A)"
        );
        assert!(
            !pty.tracks_mouse(),
            "an inline child never tracks the mouse"
        );

        // Scroll all the way back; an early committed line must be visible in
        // the composited viewport (patch B path — `cell` reads scrollback).
        pty.scroll_to_top();
        assert!(
            pty.scrollback_offset() > 0,
            "scroll_to_top must move offset"
        );
        let (rows, cols) = pty.last_size;
        // The cursor started at the bottom of the region, so the first rows to
        // scroll into history are blank; the committed `lineN` rows follow.
        // Scan the whole composited viewport for one.
        let visible: Vec<String> = (0..rows).map(|r| visible_row_text(&pty, r, cols)).collect();
        assert!(
            visible.iter().any(|line| line.starts_with("line")),
            "an early transcript line must be reachable in scrollback, got {visible:?}"
        );
    }

    #[test]
    fn wheel_leaves_the_vt100_buffer_alone_for_a_tracking_child() {
        // An alt-screen child (opencode) owns its own scroll region, so
        // wheel_up sends it a page key and must not move the host's buffer.
        let Some(sh) = resolve_on_path("sh") else {
            return;
        };
        let mut pty = PtyView::spawn(
            &sh,
            &[
                "-c".to_string(),
                "seq 1 200; printf '\\033[?1003h\\033[?1006h'; sleep 2".to_string(),
            ],
            None,
            &[],
        )
        .expect("spawn sh");

        let deadline = Instant::now() + Duration::from_millis(2000);
        while Instant::now() < deadline && !pty.tracks_mouse() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(pty.tracks_mouse(), "child never enabled tracking");

        pty.wheel_up(5);
        assert_eq!(
            pty.scrollback_offset(),
            0,
            "a tracking child owns its scroll; the host buffer must not move"
        );
    }
}

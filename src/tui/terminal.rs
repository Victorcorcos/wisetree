//! Terminal lifecycle: alt-screen, raw mode, and panic-safe restoration.
//!
//! Two flavors:
//! - `enter` renders to `stdout` (default).
//! - `enter_wrapper` renders to the controlling TTY (`/dev/tty` on Unix,
//!   `CONOUT$` on Windows). Used in `--from-wrapper` mode where real stdout
//!   is a pipe back to the shell — corrupting it with alt-screen escapes
//!   would break the `local dir=$(...)` capture.

use std::fs::{File, OpenOptions};
use std::io::{self, Stdout};

use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal as RatTerminal;

pub type Backend = CrosstermBackend<Stdout>;
pub type WrapperBackend = CrosstermBackend<File>;
pub type Terminal = RatTerminal<Backend>;
pub type WrapperTerminal = RatTerminal<WrapperBackend>;

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

/// Set raw mode + alt screen and return a ratatui terminal handle.
pub fn enter() -> io::Result<Terminal> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    RatTerminal::new(backend)
}

/// Wrapper-mode entry: render to `/dev/tty` (Unix) / `CONOUT$` (Windows)
/// instead of stdout, so the parent shell's `$(...)` capture stays clean.
pub fn enter_wrapper() -> io::Result<WrapperTerminal> {
    enable_raw_mode()?;
    let mut tty = OpenOptions::new().read(true).write(true).open(TTY_PATH)?;
    execute!(tty, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(tty);
    RatTerminal::new(backend)
}

/// Best-effort cleanup. Safe to call even if `enter` was never invoked: each
/// step ignores its own error so partial state is fully unwound.
pub fn restore() -> io::Result<()> {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    Ok(())
}

/// Best-effort cleanup for wrapper mode (re-opens the TTY to leave the alt
/// screen — we no longer hold the original handle by the time we get here).
pub fn restore_wrapper_tty() -> io::Result<()> {
    let _ = disable_raw_mode();
    if let Ok(mut tty) = OpenOptions::new().write(true).open(TTY_PATH) {
        let _ = execute!(tty, LeaveAlternateScreen);
    }
    Ok(())
}

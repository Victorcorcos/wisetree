//! Keyboard / tick event abstraction over `crossterm::event`.
//!
//! The event loop drives both rendering (via periodic `Tick`) and input.
//! `tick_rate` defaults to 100ms so the spinner widgets get a natural cadence.
//!
//! ## Dead-tty guard
//!
//! When the controlling terminal disappears (terminal tab closed, parent
//! shell killed without forwarding SIGHUP, etc.) the underlying input fd
//! enters a permanent `POLLHUP`/`POLLERR` state. In that mode:
//!
//! * crossterm 0.28's `mio` backend wedges in an infinite tight loop —
//!   `poll()` reports the fd as readable, `read()` returns `Ok(0)` (EOF),
//!   the parser stays empty, and the inner read loop has no break on
//!   `read_count == 0`. The result is a `wisetree` process pegged at 100%
//!   CPU indefinitely after the user closes the terminal tab.
//!
//! * Even crossterm's `use-dev-tty` backend, while it does break out of
//!   the inner loop on EOF, busy-spins the *outer* loop until the
//!   requested timeout elapses (50% CPU at our 50ms tick rate).
//!
//! Defense in depth:
//!
//! 1. **Pre-flight HUP probe**: before every blocking `event::poll`
//!    call we `libc::poll` the input fd with a zero timeout and check
//!    `POLLHUP`/`POLLERR`/`POLLNVAL`. If the tty is dead we report it
//!    via [`Event::TtyDisconnected`] instead of entering crossterm.
//!
//! 2. **Spin guard**: if crossterm's `event::poll` returns suspiciously
//!    fast over a streak of empty calls, we explicitly `thread::sleep`
//!    so a misbehaving backend can't peg a CPU even if the HUP probe
//!    misses something exotic.

use std::time::{Duration, Instant};

use crossterm::event::{self, Event as CtEvent, KeyEvent, MouseEvent};

/// How many consecutive instant-return polls (poll resolved in < `INSTANT_POLL_THRESHOLD`)
/// without any real input we tolerate before we assume the tty is dead and
/// stop burning CPU. Picked to be high enough that real bursty input (paste,
/// fast mouse-move events) doesn't trip the guard but low enough that a true
/// busy-spin is throttled within a few milliseconds.
const INSTANT_RETURN_LIMIT: u32 = 8;

/// A `poll` call that returns in under this is considered "instant" (didn't
/// actually wait). Threshold is generous enough to absorb scheduling jitter.
const INSTANT_POLL_THRESHOLD: Duration = Duration::from_micros(500);

/// How long to sleep once we believe the loop is spinning. The orphan
/// watchdog flips the quit flag within ~500ms of parent death, so a 50ms
/// sleep gives the watchdog plenty of room to fire without making the UI
/// feel laggy if the heuristic ever misfires under real input.
const SPIN_BACKOFF: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
    Tick,
    /// The controlling tty has gone away (POLLHUP / POLLERR on the input
    /// fd, or open(/dev/tty) failed). The event loop must break out
    /// immediately — calling crossterm again would busy-spin.
    TtyDisconnected,
}

#[derive(Debug, Clone)]
pub struct EventLoop {
    tick_rate: Duration,
    last_tick: Instant,
    spin_guard: SpinGuard,
}

impl EventLoop {
    pub fn new(tick_rate: Duration) -> Self {
        Self {
            tick_rate,
            last_tick: Instant::now(),
            spin_guard: SpinGuard::new(),
        }
    }

    /// Block until either an event is available or the tick deadline is hit.
    /// Returns the next `Event`.
    pub fn next_event(&mut self) -> std::io::Result<Event> {
        // Pre-flight: if the input fd is in POLLHUP/POLLERR we must not
        // hand control to crossterm. Doing so wedges the process at 100%
        // CPU because crossterm 0.28's mio backend has an inner read loop
        // that doesn't break on EOF.
        if tty_is_dead() {
            return Ok(Event::TtyDisconnected);
        }

        let timeout = self
            .tick_rate
            .checked_sub(self.last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        let poll_started = Instant::now();
        let has_event = event::poll(timeout)?;
        let elapsed = poll_started.elapsed();

        if has_event {
            // A real event arrived — reset the spin guard. Crossterm can
            // surface synthetic variants we don't care about (e.g. focus
            // gained/lost), which we fold into `Tick` below. Those still
            // count as a real underlying read, so resetting here is safe:
            // they consume bytes from the tty and won't loop forever.
            self.spin_guard.note_real_input();
            return Ok(match event::read()? {
                CtEvent::Key(k) => Event::Key(k),
                CtEvent::Mouse(m) => Event::Mouse(m),
                CtEvent::Resize(w, h) => Event::Resize(w, h),
                _ => Event::Tick,
            });
        }

        if self.spin_guard.note_empty_poll(timeout, elapsed) {
            std::thread::sleep(SPIN_BACKOFF);
        }

        self.last_tick = Instant::now();
        Ok(Event::Tick)
    }
}

impl Default for EventLoop {
    fn default() -> Self {
        Self::new(Duration::from_millis(100))
    }
}

/// True when the input fd (typically stdin) is in `POLLHUP`/`POLLERR`/
/// `POLLNVAL`, indicating the controlling terminal has gone away.
///
/// We probe `STDIN_FILENO` because that's the fd crossterm uses when stdin
/// is a tty (which is true for both regular and `--from-wrapper` invocations
/// — only stdout is a pipe in wrapper mode). A non-tty stdin gets the
/// `/dev/tty` fallback inside crossterm, which we can't see; the watchdog
/// in `tui::app` covers that case via its own `/dev/tty` probe.
#[cfg(unix)]
fn tty_is_dead() -> bool {
    let mut fds = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // 0 timeout = read the kernel's current readiness flags and return
    // immediately. This is roughly the cheapest syscall we can make.
    let result = unsafe { libc::poll(&mut fds, 1, 0) };
    if result < 0 {
        // poll() itself failed — assume the fd is unusable.
        return true;
    }
    fds.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

#[cfg(not(unix))]
fn tty_is_dead() -> bool {
    false
}

/// Tracks how many empty `event::poll` calls returned essentially instantly,
/// which is the signature of a dead tty (POLLHUP / POLLERR). Once the streak
/// crosses [`INSTANT_RETURN_LIMIT`] the caller throttles itself.
#[derive(Debug, Clone, Default)]
struct SpinGuard {
    instant_return_streak: u32,
}

impl SpinGuard {
    fn new() -> Self {
        Self::default()
    }

    fn note_real_input(&mut self) {
        self.instant_return_streak = 0;
    }

    /// Record an empty poll. Returns `true` when the caller should throttle.
    fn note_empty_poll(&mut self, requested_timeout: Duration, actual_elapsed: Duration) -> bool {
        if requested_timeout > INSTANT_POLL_THRESHOLD && actual_elapsed < INSTANT_POLL_THRESHOLD {
            self.instant_return_streak = self.instant_return_streak.saturating_add(1);
            self.instant_return_streak >= INSTANT_RETURN_LIMIT
        } else {
            self.instant_return_streak = 0;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_polls_that_block_for_the_full_timeout_do_not_trip_the_guard() {
        let mut guard = SpinGuard::new();
        let timeout = Duration::from_millis(50);
        // Simulate well-behaved polls that actually waited the requested
        // duration. Even a long streak should never trigger throttling.
        for _ in 0..1_000 {
            assert!(!guard.note_empty_poll(timeout, timeout));
        }
    }

    #[test]
    fn instant_empty_polls_trip_the_guard_once_the_streak_threshold_is_reached() {
        let mut guard = SpinGuard::new();
        let timeout = Duration::from_millis(50);
        let instant = Duration::from_nanos(10);

        for iteration in 0..INSTANT_RETURN_LIMIT.saturating_sub(1) {
            assert!(
                !guard.note_empty_poll(timeout, instant),
                "iteration {iteration} should not throttle yet"
            );
        }
        assert!(
            guard.note_empty_poll(timeout, instant),
            "streak at the limit should request throttling"
        );
    }

    #[test]
    fn real_input_resets_the_streak() {
        let mut guard = SpinGuard::new();
        let timeout = Duration::from_millis(50);
        let instant = Duration::from_nanos(10);

        for _ in 0..INSTANT_RETURN_LIMIT.saturating_sub(1) {
            guard.note_empty_poll(timeout, instant);
        }
        guard.note_real_input();
        // After a real input, the next instant empty poll must not be enough
        // to trigger throttling on its own.
        assert!(!guard.note_empty_poll(timeout, instant));
    }

    #[test]
    fn zero_timeout_polls_are_ignored_by_the_guard() {
        // The TUI's outer loop can request a zero timeout when previous
        // iterations overran the tick budget. We must not treat those as
        // evidence of a dead tty — they're expected catch-up work.
        let mut guard = SpinGuard::new();
        for _ in 0..(INSTANT_RETURN_LIMIT as usize * 2) {
            assert!(!guard.note_empty_poll(Duration::ZERO, Duration::from_nanos(10)));
        }
    }
}

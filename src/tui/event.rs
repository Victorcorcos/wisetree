//! Keyboard / tick event abstraction over `crossterm::event`.
//!
//! The event loop drives both rendering (via periodic `Tick`) and input.
//! The app runs it at 50ms idle so spinner widgets get a natural cadence, and
//! temporarily raises it (see [`EventLoop::set_tick_rate`]) while an embedded
//! PTY is live to keep an inline harness's animation smooth.
//!
//! ## Spin guard
//!
//! crossterm's `event::poll` can return immediately in a tight loop under
//! certain backend states. The `SpinGuard` watches for streaks of suspiciously
//! fast empty polls and inserts a short `thread::sleep` so a misbehaving
//! backend can't peg a CPU.

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

#[derive(Debug, Clone)]
pub enum Event {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Resize(u16, u16),
    /// Crossterm can surface a dead TTY as an endless streak of immediate
    /// empty polls instead of a read error or signal. Treat that as terminal
    /// loss so the app can shut down cleanly instead of lingering forever.
    Closed,
    Tick,
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

    /// Adjust the tick cadence in-flight. Used to run the render loop faster
    /// while an embedded PTY is live so an inline harness (codex/claude) whose
    /// spinner + token stream redraw quickly gets sampled smoothly, then fall
    /// back to the idle rate to spare the CPU. `last_tick` is untouched, so the
    /// next `next_event` simply recomputes its timeout against the new rate.
    pub fn set_tick_rate(&mut self, tick_rate: Duration) {
        self.tick_rate = tick_rate;
    }

    /// Block until either an event is available or the tick deadline is hit.
    /// Returns the next `Event`.
    pub fn next_event(&mut self) -> std::io::Result<Event> {
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
                CtEvent::Paste(text) => Event::Paste(text),
                CtEvent::Mouse(m) => Event::Mouse(m),
                CtEvent::Resize(w, h) => Event::Resize(w, h),
                _ => Event::Tick,
            });
        }

        if self.spin_guard.note_empty_poll(timeout, elapsed) {
            return Ok(Event::Closed);
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

/// Tracks how many empty `event::poll` calls returned essentially instantly,
/// which is the signature of a dead tty (POLLHUP / POLLERR). Once the streak
/// crosses [`INSTANT_RETURN_LIMIT`] the caller should treat the terminal as
/// gone and stop waiting for further input.
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

    /// Record an empty poll. Returns `true` when the caller should treat the
    /// TTY as dead.
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
                "iteration {iteration} should not report a dead tty yet"
            );
        }
        assert!(
            guard.note_empty_poll(timeout, instant),
            "streak at the limit should report a dead tty"
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

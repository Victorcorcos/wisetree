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

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};

/// How long a lone Escape may wait for the rest of a mouse report that
/// crossterm incorrectly split into ordinary key events.
const FRAGMENTED_MOUSE_TIMEOUT: Duration = Duration::from_millis(50);

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
    fragmented_mouse: FragmentedMouseEvents,
    pending: VecDeque<Event>,
}

impl EventLoop {
    pub fn new(tick_rate: Duration) -> Self {
        Self {
            tick_rate,
            last_tick: Instant::now(),
            spin_guard: SpinGuard::new(),
            fragmented_mouse: FragmentedMouseEvents::default(),
            pending: VecDeque::new(),
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
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(event);
            }

            let mut timeout = self
                .tick_rate
                .checked_sub(self.last_tick.elapsed())
                .unwrap_or(Duration::ZERO);
            if let Some(remaining) = self.fragmented_mouse.remaining(Instant::now()) {
                timeout = timeout.min(remaining);
            }

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
                let event = match event::read()? {
                    CtEvent::Key(key) => match self.fragmented_mouse.push(key, Instant::now()) {
                        FragmentedMouseOutcome::Pending => continue,
                        FragmentedMouseOutcome::Ready(mut events) => {
                            let first = events.pop_front().unwrap_or(Event::Tick);
                            self.pending.extend(events);
                            first
                        }
                    },
                    CtEvent::Paste(text) => self.after_fragment(Event::Paste(text)),
                    CtEvent::Mouse(mouse) => self.after_fragment(Event::Mouse(mouse)),
                    CtEvent::Resize(width, height) => {
                        self.after_fragment(Event::Resize(width, height))
                    }
                    _ => self.after_fragment(Event::Tick),
                };
                return Ok(event);
            }

            if self.spin_guard.note_empty_poll(timeout, elapsed) {
                return Ok(Event::Closed);
            }

            if let Some(mut events) = self.fragmented_mouse.take_stale(Instant::now()) {
                let first = events.pop_front().unwrap_or(Event::Tick);
                self.pending.extend(events);
                return Ok(first);
            }

            self.last_tick = Instant::now();
            return Ok(Event::Tick);
        }
    }

    fn after_fragment(&mut self, event: Event) -> Event {
        if self.fragmented_mouse.candidate.is_empty() {
            return event;
        }
        let mut events = self.fragmented_mouse.release_keys();
        events.push_back(event);
        let first = events.pop_front().unwrap_or(Event::Tick);
        self.pending.extend(events);
        first
    }
}

#[derive(Debug, Clone, Default)]
struct FragmentedMouseEvents {
    candidate: Vec<KeyEvent>,
    updated_at: Option<Instant>,
}

#[derive(Debug)]
enum FragmentedMouseOutcome {
    Pending,
    Ready(VecDeque<Event>),
}

impl FragmentedMouseEvents {
    fn push(&mut self, key: KeyEvent, now: Instant) -> FragmentedMouseOutcome {
        if self.candidate.is_empty() {
            if is_plain_press(&key, KeyCode::Esc) {
                self.candidate.push(key);
                self.updated_at = Some(now);
                return FragmentedMouseOutcome::Pending;
            }
            return FragmentedMouseOutcome::Ready(VecDeque::from([Event::Key(key)]));
        }

        self.candidate.push(key);
        self.updated_at = Some(now);
        let Some(bytes) = candidate_bytes(&self.candidate) else {
            return FragmentedMouseOutcome::Ready(self.release_keys());
        };
        match parse_sgr_mouse(&bytes) {
            SgrMouseStatus::Prefix => FragmentedMouseOutcome::Pending,
            SgrMouseStatus::Complete(mouse) => {
                self.clear();
                FragmentedMouseOutcome::Ready(VecDeque::from([Event::Mouse(mouse)]))
            }
            SgrMouseStatus::Invalid => FragmentedMouseOutcome::Ready(self.release_keys()),
        }
    }

    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.updated_at.map(|updated_at| {
            FRAGMENTED_MOUSE_TIMEOUT.saturating_sub(now.saturating_duration_since(updated_at))
        })
    }

    fn take_stale(&mut self, now: Instant) -> Option<VecDeque<Event>> {
        let updated_at = self.updated_at?;
        (now.saturating_duration_since(updated_at) >= FRAGMENTED_MOUSE_TIMEOUT)
            .then(|| self.release_keys())
    }

    fn release_keys(&mut self) -> VecDeque<Event> {
        self.updated_at = None;
        self.candidate.drain(..).map(Event::Key).collect()
    }

    fn clear(&mut self) {
        self.candidate.clear();
        self.updated_at = None;
    }
}

fn is_plain_press(key: &KeyEvent, code: KeyCode) -> bool {
    key.kind == KeyEventKind::Press && key.code == code && key.modifiers.is_empty()
}

fn candidate_bytes(keys: &[KeyEvent]) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(keys.len());
    for (index, key) in keys.iter().enumerate() {
        if key.kind != KeyEventKind::Press
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return None;
        }
        match key.code {
            KeyCode::Esc if index == 0 => bytes.push(0x1b),
            KeyCode::Char(ch) if ch.is_ascii() => bytes.push(ch as u8),
            _ => return None,
        }
    }
    Some(bytes)
}

#[derive(Debug)]
enum SgrMouseStatus {
    Prefix,
    Complete(MouseEvent),
    Invalid,
}

/// Reassembles an SGR mouse report such as `ESC [ < 64 ; 165 ; 32 M`.
/// Some macOS terminal/crossterm combinations expose each byte as a separate
/// key event under sustained wheel input. Recovering the mouse event here is
/// essential: otherwise the first byte acts as Escape in the current screen
/// and the printable tail is typed into whichever screen that opens next.
fn parse_sgr_mouse(bytes: &[u8]) -> SgrMouseStatus {
    const INTRODUCER: &[u8] = b"\x1b[<";
    const MAX_REPORT_LEN: usize = 32;

    if bytes.len() > MAX_REPORT_LEN {
        return SgrMouseStatus::Invalid;
    }
    if bytes.len() < INTRODUCER.len() {
        return if INTRODUCER.starts_with(bytes) {
            SgrMouseStatus::Prefix
        } else {
            SgrMouseStatus::Invalid
        };
    }
    if !bytes.starts_with(INTRODUCER) {
        return SgrMouseStatus::Invalid;
    }

    let body = &bytes[INTRODUCER.len()..];
    if !matches!(body.last(), Some(b'M' | b'm')) {
        return if body
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';')
        {
            SgrMouseStatus::Prefix
        } else {
            SgrMouseStatus::Invalid
        };
    }

    let released = body.last() == Some(&b'm');
    let parameters = &body[..body.len() - 1];
    let values = parameters
        .split(|byte| *byte == b';')
        .map(|field| std::str::from_utf8(field).ok()?.parse::<u16>().ok())
        .collect::<Option<Vec<_>>>();
    let Some(values) = values else {
        return SgrMouseStatus::Invalid;
    };
    let [button, column, row] = values.as_slice() else {
        return SgrMouseStatus::Invalid;
    };
    let Some(kind) = decode_mouse_kind(*button, released) else {
        return SgrMouseStatus::Invalid;
    };
    if *column == 0 || *row == 0 {
        return SgrMouseStatus::Invalid;
    }
    let mut modifiers = KeyModifiers::NONE;
    if button & 0x04 != 0 {
        modifiers.insert(KeyModifiers::SHIFT);
    }
    if button & 0x08 != 0 {
        modifiers.insert(KeyModifiers::ALT);
    }
    if button & 0x10 != 0 {
        modifiers.insert(KeyModifiers::CONTROL);
    }
    SgrMouseStatus::Complete(MouseEvent {
        kind,
        column: column - 1,
        row: row - 1,
        modifiers,
    })
}

fn decode_mouse_kind(button: u16, released: bool) -> Option<MouseEventKind> {
    let button = u8::try_from(button).ok()?;
    let button_number = (button & 0x03) | ((button & 0xc0) >> 4);
    let dragging = button & 0x20 != 0;
    let kind = match (button_number, dragging) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        (3..=5, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };
    Some(if released {
        match kind {
            MouseEventKind::Down(button) => MouseEventKind::Up(button),
            other => other,
        }
    } else {
        kind
    })
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn push_report(filter: &mut FragmentedMouseEvents, report: &str) -> VecDeque<Event> {
        let mut outcome = FragmentedMouseOutcome::Pending;
        let char_count = report.chars().count();
        for (index, ch) in report.chars().enumerate() {
            let code = if index == 0 {
                assert_eq!(ch, '\u{1b}');
                KeyCode::Esc
            } else {
                KeyCode::Char(ch)
            };
            let mut event = key(code);
            // Crossterm's Unix parser annotates standalone uppercase bytes
            // with Shift, including the final `M` in a fragmented report.
            if ch.is_ascii_uppercase() {
                event.modifiers.insert(KeyModifiers::SHIFT);
            }
            outcome = filter.push(event, Instant::now());
            if index + 1 < char_count {
                assert!(
                    matches!(outcome, FragmentedMouseOutcome::Pending),
                    "fragment {index} was released before the report completed"
                );
            }
        }
        match outcome {
            FragmentedMouseOutcome::Ready(events) => events,
            FragmentedMouseOutcome::Pending => panic!("complete report remained pending"),
        }
    }

    #[test]
    fn fragmented_wheel_report_becomes_one_mouse_event() {
        let mut filter = FragmentedMouseEvents::default();
        let mut events = push_report(&mut filter, "\u{1b}[<64;165;32M");

        let Some(Event::Mouse(mouse)) = events.pop_front() else {
            panic!("expected recovered mouse event");
        };
        assert!(events.is_empty());
        assert_eq!(mouse.kind, MouseEventKind::ScrollUp);
        assert_eq!((mouse.column, mouse.row), (164, 31));
        assert_eq!(mouse.modifiers, KeyModifiers::NONE);
        assert!(filter.candidate.is_empty());
    }

    #[test]
    fn fragmented_mouse_filter_releases_real_escape_and_following_key() {
        let mut filter = FragmentedMouseEvents::default();
        let now = Instant::now();
        assert!(matches!(
            filter.push(key(KeyCode::Esc), now),
            FragmentedMouseOutcome::Pending
        ));

        let FragmentedMouseOutcome::Ready(mut events) = filter.push(key(KeyCode::Char('x')), now)
        else {
            panic!("non-mouse input should be released");
        };
        assert!(
            matches!(events.pop_front(), Some(Event::Key(event)) if event.code == KeyCode::Esc)
        );
        assert!(
            matches!(events.pop_front(), Some(Event::Key(event)) if event.code == KeyCode::Char('x'))
        );
        assert!(events.is_empty());
    }

    #[test]
    fn fragmented_mouse_filter_releases_a_standalone_escape_after_timeout() {
        let mut filter = FragmentedMouseEvents::default();
        let now = Instant::now();
        assert!(matches!(
            filter.push(key(KeyCode::Esc), now),
            FragmentedMouseOutcome::Pending
        ));
        assert!(filter
            .take_stale(now + FRAGMENTED_MOUSE_TIMEOUT - Duration::from_millis(1))
            .is_none());

        let mut events = filter
            .take_stale(now + FRAGMENTED_MOUSE_TIMEOUT)
            .expect("escape should be released at the deadline");
        assert!(
            matches!(events.pop_front(), Some(Event::Key(event)) if event.code == KeyCode::Esc)
        );
        assert!(events.is_empty());
    }

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

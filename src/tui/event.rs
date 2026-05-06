//! Keyboard / tick event abstraction over `crossterm::event`.
//!
//! The event loop drives both rendering (via periodic `Tick`) and input.
//! `tick_rate` defaults to 100ms so the spinner widgets get a natural cadence.

use std::time::{Duration, Instant};

use crossterm::event::{self, Event as CtEvent, KeyEvent, MouseEvent};

#[derive(Debug, Clone)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
    Tick,
}

#[derive(Debug, Clone)]
pub struct EventLoop {
    tick_rate: Duration,
    last_tick: Instant,
}

impl EventLoop {
    pub fn new(tick_rate: Duration) -> Self {
        Self {
            tick_rate,
            last_tick: Instant::now(),
        }
    }

    /// Block until either an event is available or the tick deadline is hit.
    /// Returns the next `Event`. `Ok(None)` is reserved for clean shutdown
    /// signals.
    pub fn next_event(&mut self) -> std::io::Result<Event> {
        let timeout = self
            .tick_rate
            .checked_sub(self.last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        if event::poll(timeout)? {
            return Ok(match event::read()? {
                CtEvent::Key(k) => Event::Key(k),
                CtEvent::Mouse(m) => Event::Mouse(m),
                CtEvent::Resize(w, h) => Event::Resize(w, h),
                _ => Event::Tick,
            });
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

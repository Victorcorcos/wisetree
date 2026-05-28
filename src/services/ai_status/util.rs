//! Small helpers shared by every detector.

use std::time::{Duration, SystemTime};

use super::state::AiHarnessState;

/// Classify a session file's mtime as `Running` (within `window` of now) or
/// `Idle` (older than `window`). Clock skew that makes mtime appear "in the
/// future" is treated as `Running` — better than calling a live session idle.
pub fn classify_mtime(mtime: SystemTime, window: Duration) -> AiHarnessState {
    let now = SystemTime::now();
    let elapsed = now.duration_since(mtime).unwrap_or(Duration::ZERO);
    if elapsed <= window {
        AiHarnessState::Running
    } else {
        AiHarnessState::Idle
    }
}

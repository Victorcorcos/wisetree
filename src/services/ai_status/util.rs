//! Small helpers shared by every detector.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use super::state::AiHarnessState;

/// Merge a freshly-detected state into the per-cwd index. `Absent` is the
/// neutral element; otherwise the higher-ranked state wins.
pub fn merge(out: &mut BTreeMap<PathBuf, AiHarnessState>, key: PathBuf, state: AiHarnessState) {
    let entry = out.entry(key).or_insert(AiHarnessState::Absent);
    *entry = AiHarnessState::merge(*entry, state);
}

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

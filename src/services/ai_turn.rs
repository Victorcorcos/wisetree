//! Provider-neutral on-disk turn watchers for interactive AI harnesses.
//!
//! Watchers are created immediately before spawning a child. They select one
//! post-spawn transcript for the requested worktree, then keep polling only
//! that file so another terminal cannot take over an in-flight operation.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use crate::config::schema::AiHarness;

use super::ai_status::{canonical_key, AiStatusPaths};
use super::{OpencodeTurn, OpencodeTurnWatcher};

/// State of a watched interactive turn, independent of its provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiTurn {
    Working,
    /// The provider rejected the current turn because a temporary usage
    /// allowance was exhausted. The interactive CLI remains usable, so PR
    /// workflows must keep its PTY alive for a follow-up prompt after reset.
    UsageLimited {
        message: String,
    },
    Finished {
        transcript: String,
    },
    Failed {
        message: String,
    },
}

const POLL_PERIOD: Duration = Duration::from_millis(1000);
const MAX_TRANSCRIPT_BYTES: u64 = 4 * 1024 * 1024;

/// Watch the interactive turn for one configured provider.
pub struct AiTurnWatcher(AiTurnWatcherKind);

enum AiTurnWatcherKind {
    OpenCode(OpencodeTurnWatcher),
    Codex(JsonlTurnWatcher),
    Claude(JsonlTurnWatcher),
}

impl AiTurnWatcher {
    /// Create before spawning the interactive child.
    pub fn new(harness: AiHarness, worktree: &Path) -> Self {
        Self::with_paths(
            harness,
            worktree,
            AiStatusPaths::detect(),
            SystemTime::now(),
        )
    }

    /// Test seam for hermetic state directories and a fixed spawn time.
    pub fn with_paths(
        harness: AiHarness,
        worktree: &Path,
        paths: AiStatusPaths,
        since: SystemTime,
    ) -> Self {
        match harness {
            AiHarness::OpenCode => Self(AiTurnWatcherKind::OpenCode(
                OpencodeTurnWatcher::with_db_path(
                    paths.opencode_data.map(|dir| dir.join("opencode.db")),
                    worktree,
                    since
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map(|duration| duration.as_millis() as i64)
                        .unwrap_or(0),
                ),
            )),
            AiHarness::Codex => Self(AiTurnWatcherKind::Codex(JsonlTurnWatcher::new(
                JsonlHarness::Codex,
                paths.codex_sessions,
                worktree,
                since,
            ))),
            AiHarness::ClaudeCode => Self(AiTurnWatcherKind::Claude(JsonlTurnWatcher::new(
                JsonlHarness::Claude,
                paths.claude_projects,
                worktree,
                since,
            ))),
        }
    }

    /// Poll at most once per second; use [`Self::check_now`] for exit and
    /// manual-continuation edges.
    pub fn poll(&mut self) -> Option<AiTurn> {
        match &mut self.0 {
            AiTurnWatcherKind::OpenCode(watcher) => watcher.poll().map(Into::into),
            AiTurnWatcherKind::Codex(watcher) | AiTurnWatcherKind::Claude(watcher) => {
                watcher.poll()
            }
        }
    }

    pub fn check_now(&mut self) -> AiTurn {
        match &mut self.0 {
            AiTurnWatcherKind::OpenCode(watcher) => watcher.check_now().into(),
            AiTurnWatcherKind::Codex(watcher) | AiTurnWatcherKind::Claude(watcher) => {
                watcher.check_now()
            }
        }
    }

    /// Returns the selected turn's available assistant text even while it is
    /// still streaming, for manual continuation and PTY-exit recovery.
    pub fn transcript_now(&mut self) -> Option<String> {
        match &mut self.0 {
            AiTurnWatcherKind::OpenCode(watcher) => watcher.transcript_now(),
            AiTurnWatcherKind::Codex(watcher) | AiTurnWatcherKind::Claude(watcher) => {
                watcher.transcript_now()
            }
        }
    }
}

impl From<OpencodeTurn> for AiTurn {
    fn from(turn: OpencodeTurn) -> Self {
        match turn {
            OpencodeTurn::Working => Self::Working,
            OpencodeTurn::Finished { transcript } => Self::Finished { transcript },
            OpencodeTurn::Failed { message } if is_usage_limit_error(&message) => {
                Self::UsageLimited { message }
            }
            OpencodeTurn::Failed { message } => Self::Failed { message },
        }
    }
}

/// Provider-neutral recognition of temporary account/rate allowances. Keep
/// this deliberately narrower than generic "token limit" wording: context
/// window failures require changing the prompt/session and are not cured by
/// waiting in the existing PTY.
fn is_usage_limit_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("usage limit")
        || message.contains("session limit")
        || message.contains("weekly limit")
        || message.contains("rate limit")
        || message.contains("rate_limit")
        || message.contains("ratelimit")
        || message.contains("quota exceeded")
        || message.contains("exceeded your current quota")
        || message.contains("insufficient_quota")
        || message.contains("too many requests")
        || message.contains("http 429")
        || (message.contains("limit") && message.contains("reset"))
}

#[derive(Clone, Copy)]
enum JsonlHarness {
    Codex,
    Claude,
}

struct JsonlTurnWatcher {
    harness: JsonlHarness,
    root: Option<PathBuf>,
    directory: PathBuf,
    since: SystemTime,
    /// File identity is the pin: we never rescan after this has been selected.
    file: Option<PathBuf>,
    last_poll: Option<Instant>,
}

impl JsonlTurnWatcher {
    fn new(
        harness: JsonlHarness,
        root: Option<PathBuf>,
        worktree: &Path,
        since: SystemTime,
    ) -> Self {
        Self {
            harness,
            root,
            directory: canonical_key(worktree),
            since,
            file: None,
            last_poll: None,
        }
    }

    fn poll(&mut self) -> Option<AiTurn> {
        if self
            .last_poll
            .is_some_and(|last| last.elapsed() < POLL_PERIOD)
        {
            return None;
        }
        self.last_poll = Some(Instant::now());
        Some(self.check_now())
    }

    fn check_now(&mut self) -> AiTurn {
        let Some(file) = self.resolve_file() else {
            return AiTurn::Working;
        };
        match read_jsonl_turn(self.harness, &file) {
            Ok(Some(turn)) => turn,
            // A writer can leave one incomplete/malformed final line. It is
            // not a completion signal and must not block the render loop.
            Ok(None) | Err(_) => AiTurn::Working,
        }
    }

    fn transcript_now(&mut self) -> Option<String> {
        let file = self.resolve_file()?;
        read_jsonl_turn(self.harness, &file)
            .ok()
            .flatten()
            .and_then(|turn| match turn {
                AiTurn::Finished { transcript } => Some(transcript),
                AiTurn::Working => read_assistant_text(self.harness, &file).ok(),
                AiTurn::UsageLimited { .. } => read_assistant_text(self.harness, &file).ok(),
                AiTurn::Failed { .. } => read_assistant_text(self.harness, &file).ok(),
            })
    }

    fn resolve_file(&mut self) -> Option<PathBuf> {
        if let Some(file) = &self.file {
            return Some(file.clone());
        }
        let root = self.root.as_ref()?;
        let candidates = match self.harness {
            JsonlHarness::Codex => codex_files(root),
            JsonlHarness::Claude => claude_files(root),
        };
        let mut selected = None;
        for path in candidates {
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if modified < self.since || metadata.len() > MAX_TRANSCRIPT_BYTES {
                continue;
            }
            let Ok(Some(cwd)) = read_cwd(self.harness, &path) else {
                continue;
            };
            if canonical_key(Path::new(&cwd)) != self.directory {
                continue;
            }
            if !matches!(selected.as_ref(), Some((_, previous)) if modified <= *previous) {
                selected = Some((path, modified));
            }
        }
        let (path, _) = selected?;
        self.file = Some(path.clone());
        Some(path)
    }
}

fn codex_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(years) = fs::read_dir(root) else {
        return files;
    };
    for year in years.flatten() {
        let Ok(months) = fs::read_dir(year.path()) else {
            continue;
        };
        for month in months.flatten() {
            let Ok(days) = fs::read_dir(month.path()) else {
                continue;
            };
            for day in days.flatten() {
                let Ok(entries) = fs::read_dir(day.path()) else {
                    continue;
                };
                files.extend(entries.flatten().filter_map(|entry| {
                    let path = entry.path();
                    (path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")).then_some(path)
                }));
            }
        }
    }
    files
}

fn claude_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(projects) = fs::read_dir(root) else {
        return files;
    };
    for project in projects.flatten() {
        let Ok(entries) = fs::read_dir(project.path()) else {
            continue;
        };
        files.extend(entries.flatten().filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")).then_some(path)
        }));
    }
    files
}

fn read_jsonl_turn(harness: JsonlHarness, file: &Path) -> std::io::Result<Option<AiTurn>> {
    let lines = json_lines(file)?;
    let cwd = lines.iter().find_map(|line| cwd_from_line(harness, line));
    let Some(_) = cwd else { return Ok(None) };
    let turn = match harness {
        JsonlHarness::Codex => codex_turn(&lines),
        JsonlHarness::Claude => claude_turn(&lines),
    };
    Ok(Some(turn))
}

fn read_cwd(harness: JsonlHarness, file: &Path) -> std::io::Result<Option<String>> {
    Ok(json_lines(file)?
        .iter()
        .find_map(|line| cwd_from_line(harness, line)))
}

fn read_assistant_text(harness: JsonlHarness, file: &Path) -> std::io::Result<String> {
    let lines = json_lines(file)?;
    Ok(match harness {
        JsonlHarness::Codex => codex_text(&lines),
        JsonlHarness::Claude => claude_text(&lines),
    })
}

fn json_lines(file: &Path) -> std::io::Result<Vec<Value>> {
    if fs::metadata(file)?.len() > MAX_TRANSCRIPT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "transcript exceeds turn watcher limit",
        ));
    }
    let file = fs::File::open(file)?;
    let mut lines = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if let Ok(value) = serde_json::from_str(&line) {
            lines.push(value);
        }
    }
    Ok(lines)
}

fn cwd_from_line(harness: JsonlHarness, line: &Value) -> Option<String> {
    match harness {
        JsonlHarness::Codex => line
            .pointer("/payload/cwd")
            .or_else(|| line.pointer("/payload/payload/cwd")),
        JsonlHarness::Claude => line.get("cwd"),
    }
    .and_then(Value::as_str)
    .filter(|cwd| !cwd.trim().is_empty())
    .map(str::to_string)
}

fn codex_turn(lines: &[Value]) -> AiTurn {
    let mut active = false;
    let mut completed = false;
    let mut failure: Option<String> = None;
    let mut fallback_text: Option<String> = None;
    let mut text = Vec::new();
    for line in lines {
        let kind = line.pointer("/payload/type").and_then(Value::as_str);
        if line.get("type").and_then(Value::as_str) == Some("response_item") && active {
            text.extend(codex_line_text(line));
            continue;
        }
        if line.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        match kind {
            Some("task_started") | Some("turn_started") => {
                active = true;
                completed = false;
                failure = None;
                fallback_text = None;
                text.clear();
            }
            Some("task_complete") | Some("turn_complete") if active => {
                completed = true;
                fallback_text = line
                    .pointer("/payload/last_agent_message")
                    .and_then(Value::as_str)
                    .filter(|message| !message.trim().is_empty())
                    .map(str::to_string);
            }
            // An Esc-interrupt in the interactive TUI emits `turn_aborted`.
            // That is the user stopping to redirect, not a finished or failed
            // turn — keep it `Working` so a follow-up prompt (which writes a
            // fresh `task_started`) resumes the same session instead of the
            // watcher tearing it down. Mirrors Claude Code, whose interrupts
            // likewise leave the turn `Working`. A genuine failure still
            // arrives as `error`/`task_failed` below. An early quit with no
            // resume is caught by the PTY-exit handlers, not here.
            Some("turn_aborted") if active => {
                completed = false;
                failure = None;
                fallback_text = None;
                text.clear();
            }
            Some("error") | Some("task_failed") if active => {
                failure = Some(
                    codex_error_message(line)
                        .unwrap_or_else(|| "Codex aborted the turn.".to_string()),
                );
            }
            _ => {}
        }
    }
    if let Some(message) = failure {
        if is_usage_limit_error(&message) {
            return AiTurn::UsageLimited { message };
        }
        return AiTurn::Failed { message };
    }
    if !active || !completed {
        return AiTurn::Working;
    }
    let transcript = fallback_text.or_else(|| (!text.is_empty()).then(|| text.join("\n\n")));
    match transcript {
        Some(transcript) => AiTurn::Finished { transcript },
        None => AiTurn::Working,
    }
}

fn codex_error_message(line: &Value) -> Option<String> {
    line.pointer("/payload/message")
        .or_else(|| line.pointer("/payload/error/message"))
        .or_else(|| line.pointer("/payload/error"))
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .map(str::to_string)
}

fn codex_line_text(line: &Value) -> Vec<String> {
    line.pointer("/payload")
        .filter(|payload| {
            payload.get("type").and_then(Value::as_str) == Some("message")
                && payload.get("role").and_then(Value::as_str) == Some("assistant")
        })
        .and_then(|payload| payload.get("content").and_then(Value::as_array))
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn codex_text(lines: &[Value]) -> String {
    let start = lines
        .iter()
        .rposition(|line| {
            matches!(
                line.pointer("/payload/type").and_then(Value::as_str),
                Some("task_started" | "turn_started")
            )
        })
        .unwrap_or(0);
    lines[start..]
        .iter()
        .filter(|line| line.get("type").and_then(Value::as_str) == Some("response_item"))
        .flat_map(codex_line_text)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn claude_turn(lines: &[Value]) -> AiTurn {
    let mut state = None;
    for line in lines {
        if line.get("type").and_then(Value::as_str) == Some("user")
            && line.get("promptId").is_some()
        {
            state = Some(Ok(ClaudeTurnState::Working));
        }
        if line.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if line.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
            let message = claude_line_text(line)
                .unwrap_or_else(|| "Claude Code reported an API error.".to_string());
            state = Some(if is_usage_limit_error(&message) {
                Ok(ClaudeTurnState::UsageLimited(message))
            } else {
                Err(message)
            });
            continue;
        }
        match line.pointer("/message/stop_reason").and_then(Value::as_str) {
            Some("tool_use") => state = Some(Ok(ClaudeTurnState::Working)),
            Some("error") | Some("aborted") => {
                state = Some(Err("Claude Code aborted the turn.".to_string()))
            }
            Some(_) if claude_line_text(line).is_some() => {
                state = Some(Ok(ClaudeTurnState::Finished))
            }
            Some(_) => state = Some(Ok(ClaudeTurnState::Working)),
            None => {}
        }
    }
    match state {
        Some(Ok(ClaudeTurnState::Working)) | None => AiTurn::Working,
        Some(Ok(ClaudeTurnState::UsageLimited(message))) => AiTurn::UsageLimited { message },
        Some(Err(message)) => AiTurn::Failed { message },
        Some(Ok(ClaudeTurnState::Finished)) => AiTurn::Finished {
            transcript: claude_text(lines),
        },
    }
}

enum ClaudeTurnState {
    Working,
    UsageLimited(String),
    Finished,
}

fn claude_line_text(line: &Value) -> Option<String> {
    let text = line
        .pointer("/message/content")
        .and_then(Value::as_array)?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

fn claude_text(lines: &[Value]) -> String {
    lines
        .iter()
        .filter(|line| line.get("type").and_then(Value::as_str) == Some("assistant"))
        .filter_map(|line| line.pointer("/message/content").and_then(Value::as_array))
        .flatten()
        .filter_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
                .map(str::to_string)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn paths(tmp: &tempfile::TempDir) -> AiStatusPaths {
        AiStatusPaths {
            codex_sessions: Some(tmp.path().join("codex")),
            claude_projects: Some(tmp.path().join("claude")),
            ..Default::default()
        }
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn codex_lifecycle_pins_one_post_spawn_file_and_recovers_partial_text() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("worktree");
        fs::create_dir(&worktree).unwrap();
        let file = tmp.path().join("codex/2026/01/01/ours.jsonl");
        write(&file, &format!("{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\"}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"partial\"}}]}}}}", worktree.display()));
        let since = SystemTime::now() - Duration::from_secs(1);
        let mut watcher =
            AiTurnWatcher::with_paths(AiHarness::Codex, &worktree, paths(&tmp), since);
        assert_eq!(watcher.check_now(), AiTurn::Working);
        assert_eq!(watcher.transcript_now().as_deref(), Some("partial"));
        fs::write(
            &file,
            format!(
                "{}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}",
                fs::read_to_string(&file).unwrap()
            ),
        )
        .unwrap();
        assert_eq!(
            watcher.check_now(),
            AiTurn::Finished {
                transcript: "partial".into()
            }
        );
    }

    #[test]
    fn codex_esc_abort_stays_working_and_a_followup_prompt_resumes_the_same_session() {
        // An Esc-interrupt (turn_aborted) is the user redirecting, not a
        // finished/failed turn: the watcher must stay Working so a follow-up
        // prompt continues the session instead of being torn down. A genuine
        // failure (task_failed) is still reported as Failed. Also guards the
        // pin: an unrelated newer file cannot hijack the in-flight turn.
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("worktree");
        fs::create_dir(&worktree).unwrap();
        let ours = tmp.path().join("codex/2026/01/01/ours.jsonl");
        write(&ours, &format!("{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\"}}}}", worktree.display()));
        let mut watcher = AiTurnWatcher::with_paths(
            AiHarness::Codex,
            &worktree,
            paths(&tmp),
            SystemTime::now() - Duration::from_secs(1),
        );
        assert_eq!(watcher.check_now(), AiTurn::Working);
        write(&tmp.path().join("codex/2026/01/01/newer.jsonl"), &format!("{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}", worktree.display()));
        assert_eq!(watcher.check_now(), AiTurn::Working);
        // Esc pressed → turn_aborted. Still Working, not Failed.
        fs::write(
            &ours,
            format!(
                "{}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"turn_aborted\"}}}}",
                fs::read_to_string(&ours).unwrap()
            ),
        )
        .unwrap();
        assert_eq!(watcher.check_now(), AiTurn::Working);
        // A follow-up prompt writes a fresh task_started → keeps working.
        fs::write(
            &ours,
            format!(
                "{}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\"}}}}",
                fs::read_to_string(&ours).unwrap()
            ),
        )
        .unwrap();
        assert_eq!(watcher.check_now(), AiTurn::Working);
        // That resumed turn genuinely completing is still detected as Finished.
        fs::write(
            &ours,
            format!(
                "{}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"done\"}}]}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}",
                fs::read_to_string(&ours).unwrap()
            ),
        )
        .unwrap();
        assert!(matches!(watcher.check_now(), AiTurn::Finished { .. }));
    }

    #[test]
    fn codex_genuine_failure_is_still_reported() {
        // The abort → Working change must not mask real failures: an
        // `error`/`task_failed` event still surfaces as Failed.
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("worktree");
        fs::create_dir(&worktree).unwrap();
        let file = tmp.path().join("codex/2026/01/01/ours.jsonl");
        write(&file, &format!("{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_failed\"}}}}", worktree.display()));
        let mut watcher = AiTurnWatcher::with_paths(
            AiHarness::Codex,
            &worktree,
            paths(&tmp),
            SystemTime::now() - Duration::from_secs(1),
        );
        assert!(matches!(watcher.check_now(), AiTurn::Failed { .. }));
    }

    #[test]
    fn codex_usage_limit_pauses_until_a_followup_turn_completes() {
        let mut lines = vec![
            serde_json::json!({"type": "event_msg", "payload": {"type": "task_started"}}),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "error", "message": "You've hit your usage limit"}
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_complete", "last_agent_message": null}
            }),
        ];

        assert_eq!(
            codex_turn(&lines),
            AiTurn::UsageLimited {
                message: "You've hit your usage limit".to_string()
            }
        );

        lines.extend([
            serde_json::json!({"type": "event_msg", "payload": {"type": "task_started"}}),
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "task_complete",
                    "last_agent_message": "continued after reset"
                }
            }),
        ]);
        assert_eq!(
            codex_turn(&lines),
            AiTurn::Finished {
                transcript: "continued after reset".to_string()
            }
        );
    }

    #[test]
    fn codex_textless_internal_turn_does_not_reuse_an_older_answer() {
        let mut lines = vec![
            serde_json::json!({"type": "event_msg", "payload": {"type": "task_started"}}),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "old answer"}]
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_complete", "last_agent_message": "old answer"}
            }),
            serde_json::json!({"type": "event_msg", "payload": {"type": "task_started"}}),
            serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_complete", "last_agent_message": null}
            }),
        ];

        assert_eq!(codex_turn(&lines), AiTurn::Working);
        assert_eq!(codex_text(&lines), "");

        lines.push(serde_json::json!({"type": "event_msg", "payload": {"type": "task_started"}}));
        lines.push(serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "task_complete", "last_agent_message": "final answer"}
        }));
        assert_eq!(
            codex_turn(&lines),
            AiTurn::Finished {
                transcript: "final answer".to_string()
            }
        );
    }

    #[test]
    fn claude_tool_use_waits_but_terminal_stop_returns_only_text_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("worktree");
        fs::create_dir(&worktree).unwrap();
        let file = tmp.path().join("claude/project/session.jsonl");
        write(&file, &format!("{{\"type\":\"user\",\"promptId\":\"p\",\"cwd\":\"{}\"}}\n{{\"type\":\"assistant\",\"cwd\":\"{}\",\"message\":{{\"stop_reason\":\"tool_use\",\"content\":[{{\"type\":\"text\",\"text\":\"calling tool\"}},{{\"type\":\"tool_use\"}}]}}}}", worktree.display(), worktree.display()));
        let mut watcher = AiTurnWatcher::with_paths(
            AiHarness::ClaudeCode,
            &worktree,
            paths(&tmp),
            SystemTime::now() - Duration::from_secs(1),
        );
        assert_eq!(watcher.check_now(), AiTurn::Working);
        fs::write(&file, format!("{}\n{{\"type\":\"assistant\",\"cwd\":\"{}\",\"message\":{{\"stop_reason\":\"end_turn\",\"content\":[{{\"type\":\"text\",\"text\":\"done\"}}]}}}}", fs::read_to_string(&file).unwrap(), worktree.display())).unwrap();
        assert_eq!(
            watcher.check_now(),
            AiTurn::Finished {
                transcript: "calling tool\n\ndone".into()
            }
        );
    }

    #[test]
    fn claude_thinking_only_terminal_snapshot_waits_for_text() {
        let lines = vec![
            serde_json::json!({"type": "user", "promptId": "p"}),
            serde_json::json!({
                "type": "assistant",
                "message": {
                    "id": "message-1",
                    "stop_reason": "tool_use",
                    "content": [{"type": "text", "text": "exploring"}]
                }
            }),
            serde_json::json!({
                "type": "assistant",
                "message": {
                    "id": "message-1",
                    "stop_reason": "end_turn",
                    "content": [{"type": "thinking", "thinking": ""}]
                }
            }),
        ];

        assert_eq!(claude_turn(&lines), AiTurn::Working);

        let mut completed = lines;
        completed.push(serde_json::json!({
            "type": "assistant",
            "message": {
                "id": "message-1",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "==== TASK ===="}]
            }
        }));
        assert_eq!(
            claude_turn(&completed),
            AiTurn::Finished {
                transcript: "exploring\n\n==== TASK ====".to_string()
            }
        );
    }

    #[test]
    fn claude_api_limit_pauses_until_a_followup_turn_completes() {
        let mut lines = vec![
            serde_json::json!({"type": "user", "promptId": "p"}),
            serde_json::json!({
                "type": "assistant",
                "isApiErrorMessage": true,
                "error": "rate_limit",
                "message": {
                    "stop_reason": "stop_sequence",
                    "content": [{
                        "type": "text",
                        "text": "You've hit your weekly limit · resets 6am"
                    }]
                }
            }),
        ];

        assert_eq!(
            claude_turn(&lines),
            AiTurn::UsageLimited {
                message: "You've hit your weekly limit · resets 6am".to_string()
            }
        );

        lines.extend([
            serde_json::json!({"type": "user", "promptId": "continued"}),
            serde_json::json!({
                "type": "assistant",
                "message": {
                    "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": "done after reset"}]
                }
            }),
        ]);
        assert_eq!(
            claude_turn(&lines),
            AiTurn::Finished {
                transcript: "You've hit your weekly limit · resets 6am\n\ndone after reset"
                    .to_string()
            }
        );
    }

    #[test]
    fn opencode_usage_limit_maps_to_the_non_terminal_state() {
        let turn = AiTurn::from(OpencodeTurn::Failed {
            message: "Provider returned HTTP 429: Too Many Requests".to_string(),
        });

        assert!(matches!(turn, AiTurn::UsageLimited { .. }));
        assert!(matches!(
            AiTurn::from(OpencodeTurn::Failed {
                message: "model unavailable".to_string()
            }),
            AiTurn::Failed { .. }
        ));
        assert!(!is_usage_limit_error(
            "This model's context token limit was exceeded"
        ));
    }
}

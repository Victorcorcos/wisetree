//! Detect when an embedded opencode TUI finishes its turn, from disk.
//!
//! The opencode TUI never exits on its own, so a screen that embeds it (the
//! Bugkill investigation) needs an external "the AI is done" signal to move
//! on without asking the user to confirm. opencode's in-memory
//! `session.status: idle` event is never persisted, but every turn leaves a
//! durable marker in `${XDG_DATA_HOME}/opencode/opencode.db`: the assistant
//! message's `time.completed` is written by `SessionProcessor.cleanup()`,
//! which runs under `Effect.ensuring(...)` — on success, on error, and on
//! abort alike. That makes it the most reliable on-disk completion signal
//! (the `step-finish`/`stop` part the dashboard's `ai_status` scanner keys
//! on is written per LLM step and is skipped when a turn dies mid-step).
//!
//! [`OpencodeTurnWatcher`] binds to one worktree at spawn time, finds the
//! session the embedded TUI creates (`directory` match + `time_created` at
//! or after the watcher's start, so a retry never latches onto the previous
//! run's session), and classifies the newest message:
//!
//! - no session / newest message is a user prompt / an unfinished assistant
//!   turn → [`OpencodeTurn::Working`]
//! - assistant `summary` message (auto-compaction in flight) → `Working`
//! - assistant with `time.completed` + `error` → [`OpencodeTurn::Failed`]
//! - assistant with `time.completed` + terminal `finish` → [`OpencodeTurn::Finished`], carrying
//!   the transcript (the session's assistant `text` parts, in order) — the
//!   PTY capture of a TUI is escape-sequence soup, so the database is also
//!   the transcript source.
//!
//! Reusable by any screen that embeds the opencode TUI and wants to advance
//! automatically when the turn ends.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::ai_status::{canonical_key, AiStatusPaths};

/// State of the watched opencode turn, as recorded on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpencodeTurn {
    /// No matching session yet, or the newest turn is still streaming.
    Working,
    /// The newest assistant turn completed cleanly. `transcript` is the
    /// concatenated assistant text of the whole session.
    Finished { transcript: String },
    /// The newest assistant turn completed with a recorded session error.
    Failed { message: String },
}

/// At most one database read per second — the App polls on its 100 ms tick.
const POLL_PERIOD: Duration = Duration::from_millis(1000);

pub struct OpencodeTurnWatcher {
    db_path: Option<PathBuf>,
    /// Canonicalized worktree the embedded TUI runs in.
    directory: PathBuf,
    /// Only sessions created at/after this epoch-ms count as "ours".
    since_ms: i64,
    /// Pinned once resolved, so a later session in the same directory
    /// (the user opening their own opencode) can't hijack the watch.
    session_id: Option<String>,
    last_poll: Option<Instant>,
}

impl OpencodeTurnWatcher {
    /// Watch the session an about-to-be-spawned opencode TUI will create in
    /// `worktree`. Call **before** spawning so the start timestamp precedes
    /// the session row.
    pub fn new(worktree: &Path) -> Self {
        let db_path = AiStatusPaths::detect()
            .opencode_data
            .map(|dir| dir.join("opencode.db"));
        Self::with_db_path(db_path, worktree, now_ms())
    }

    /// Test seam: explicit database path and start timestamp.
    pub fn with_db_path(db_path: Option<PathBuf>, worktree: &Path, since_ms: i64) -> Self {
        Self {
            db_path,
            directory: canonical_key(worktree),
            since_ms,
            session_id: None,
            last_poll: None,
        }
    }

    /// Throttled poll for a per-frame tick loop: reads the database at most
    /// once per [`POLL_PERIOD`], returning `None` in between.
    pub fn poll(&mut self) -> Option<OpencodeTurn> {
        if let Some(last) = self.last_poll {
            if last.elapsed() < POLL_PERIOD {
                return None;
            }
        }
        self.last_poll = Some(Instant::now());
        Some(self.check_now())
    }

    /// Unthrottled check — for edges (PTY exit, user-forced continue) where
    /// the caller needs an answer immediately.
    pub fn check_now(&mut self) -> OpencodeTurn {
        let Some(conn) = self.open() else {
            return OpencodeTurn::Working;
        };
        let Some(session_id) = self.resolve_session(&conn) else {
            return OpencodeTurn::Working;
        };
        let Some(message) = latest_message(&conn, &session_id) else {
            return OpencodeTurn::Working;
        };
        if message.role != "assistant" || message.summary || message.completed.is_none() {
            return OpencodeTurn::Working;
        }
        if let Some(error) = message.error {
            // opencode stamps `time.completed` even on abort (its
            // `SessionProcessor.cleanup` runs on success, error, and abort
            // alike) and tags a user Esc-interrupt as `MessageAbortedError`.
            // That is the user interrupting to redirect, not a finished or
            // failed turn — keep it `Working` so a follow-up prompt continues
            // the same session, mirroring Codex `turn_aborted` and Claude Code
            // interrupts. A genuine provider/model error (a different
            // `error.name`) still ends the turn as `Failed`.
            if is_abort_error(message.error_name.as_deref()) {
                return OpencodeTurn::Working;
            }
            return OpencodeTurn::Failed { message: error };
        }
        if message
            .finish
            .as_deref()
            .map_or(true, |finish| finish == "tool-calls")
        {
            return OpencodeTurn::Working;
        }
        OpencodeTurn::Finished {
            transcript: transcript(&conn, &session_id).unwrap_or_default(),
        }
    }

    /// The session's assistant text so far, regardless of completion — the
    /// escape hatch behind the user-forced continue when the completion
    /// marker is missing but the reply visibly landed.
    pub fn transcript_now(&mut self) -> Option<String> {
        let conn = self.open()?;
        let session_id = self.resolve_session(&conn)?;
        transcript(&conn, &session_id)
    }

    /// Read-only open; a missing or momentarily locked database is simply
    /// "no signal yet", never an error.
    fn open(&self) -> Option<Connection> {
        let path = self.db_path.as_ref()?;
        if !path.exists() {
            return None;
        }
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
    }

    fn resolve_session(&mut self, conn: &Connection) -> Option<String> {
        if let Some(id) = self.session_id.as_ref() {
            return Some(id.clone());
        }
        let mut stmt = conn
            .prepare(
                "select id, directory from session \
                 where time_created >= ?1 \
                 order by time_created desc, id desc \
                 limit 50",
            )
            .ok()?;
        let rows = stmt
            .query_map([self.since_ms], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .ok()?;
        for row in rows.flatten() {
            let (id, directory) = row;
            if canonical_key(Path::new(&directory)) == self.directory {
                self.session_id = Some(id.clone());
                return Some(id);
            }
        }
        None
    }
}

/// Whether an `error.name` denotes a user-initiated abort (Esc-interrupt /
/// Ctrl-C) rather than a real failure. opencode names these `MessageAbortedError`;
/// matching on the `Abort` stem also covers the shorter `AbortedError` variant.
fn is_abort_error(error_name: Option<&str>) -> bool {
    error_name.is_some_and(|name| name.contains("Abort"))
}

struct MessageMarker {
    role: String,
    summary: bool,
    completed: Option<i64>,
    error: Option<String>,
    /// The raw `error.name` (e.g. `MessageAbortedError`), kept separately from
    /// the display `error` so [`is_abort_error`] can tell a user Esc-interrupt
    /// apart from a genuine provider/model failure.
    error_name: Option<String>,
    /// OpenCode's per-step finish reason. `tool-calls` continues the loop;
    /// terminal values such as `stop` finish the user turn.
    finish: Option<String>,
}

fn latest_message(conn: &Connection, session_id: &str) -> Option<MessageMarker> {
    let mut stmt = conn
        .prepare(
            "select json_extract(data, '$.role'), \
                    coalesce(json_extract(data, '$.summary'), 0), \
                    json_extract(data, '$.time.completed'), \
                    coalesce(json_extract(data, '$.error.data.message'), \
                             json_extract(data, '$.error.name')), \
                    json_extract(data, '$.error.name'), \
                    json_extract(data, '$.finish') \
             from message \
             where session_id = ?1 \
             order by time_created desc, id desc \
             limit 1",
        )
        .ok()?;
    stmt.query_row([session_id], |row| {
        Ok(MessageMarker {
            role: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            summary: row.get::<_, i64>(1)? != 0,
            completed: row.get(2)?,
            error: row.get(3)?,
            error_name: row.get(4)?,
            finish: row.get(5)?,
        })
    })
    .optional()
    .ok()
    .flatten()
}

/// Concatenated `text` parts of the session's assistant messages (summary /
/// compaction messages excluded), in message + part order. This is the same
/// prose a non-interactive `opencode run` would have printed, so downstream
/// contract parsers work unchanged.
fn transcript(conn: &Connection, session_id: &str) -> Option<String> {
    let mut stmt = conn
        .prepare(
            "select json_extract(p.data, '$.text') \
             from part p \
             join message m on m.id = p.message_id \
             where m.session_id = ?1 \
               and json_extract(m.data, '$.role') = 'assistant' \
               and coalesce(json_extract(m.data, '$.summary'), 0) = 0 \
               and json_extract(p.data, '$.type') = 'text' \
             order by m.time_created asc, m.id asc, p.id asc",
        )
        .ok()?;
    let rows = stmt
        .query_map([session_id], |row| row.get::<_, Option<String>>(0))
        .ok()?;
    let texts: Vec<String> = rows.flatten().flatten().collect();
    Some(texts.join("\n\n"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use std::path::PathBuf;

    /// Mirrors the columns of opencode's drizzle schema
    /// (`packages/core/src/session/sql.ts`) that the watcher queries.
    fn create_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "create table session (
                 id text primary key,
                 directory text not null,
                 time_created integer not null,
                 time_updated integer not null
             );
             create table message (
                 id text primary key,
                 session_id text not null,
                 time_created integer not null,
                 time_updated integer not null,
                 data text not null
             );
             create table part (
                 id text primary key,
                 message_id text not null,
                 session_id text not null,
                 time_created integer not null,
                 time_updated integer not null,
                 data text not null
             );",
        )
        .unwrap();
        conn
    }

    fn insert_session(conn: &Connection, id: &str, directory: &Path, created_ms: i64) {
        conn.execute(
            "insert into session (id, directory, time_created, time_updated) \
             values (?1, ?2, ?3, ?3)",
            params![id, directory.to_string_lossy(), created_ms],
        )
        .unwrap();
    }

    fn insert_message(conn: &Connection, id: &str, session_id: &str, created_ms: i64, data: &str) {
        conn.execute(
            "insert into message (id, session_id, time_created, time_updated, data) \
             values (?1, ?2, ?3, ?3, ?4)",
            params![id, session_id, created_ms, data],
        )
        .unwrap();
    }

    fn insert_part(conn: &Connection, id: &str, message_id: &str, session_id: &str, data: &str) {
        conn.execute(
            "insert into part (id, message_id, session_id, time_created, time_updated, data) \
             values (?1, ?2, ?3, 0, 0, ?4)",
            params![id, message_id, session_id, data],
        )
        .unwrap();
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        conn: Connection,
        db_path: PathBuf,
        worktree: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let db_path = tmp.path().join("opencode.db");
        let conn = create_db(&db_path);
        Fixture {
            _tmp: tmp,
            conn,
            db_path,
            worktree,
        }
    }

    fn watcher(f: &Fixture, since_ms: i64) -> OpencodeTurnWatcher {
        OpencodeTurnWatcher::with_db_path(Some(f.db_path.clone()), &f.worktree, since_ms)
    }

    #[test]
    fn missing_db_and_missing_session_report_working() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w =
            OpencodeTurnWatcher::with_db_path(Some(tmp.path().join("absent.db")), tmp.path(), 0);
        assert_eq!(w.check_now(), OpencodeTurn::Working);
        assert_eq!(w.transcript_now(), None);

        let f = fixture();
        let mut w = watcher(&f, 1_000);
        assert_eq!(w.check_now(), OpencodeTurn::Working);
    }

    #[test]
    fn poll_throttles_to_one_read_per_period() {
        let f = fixture();
        let mut w = watcher(&f, 0);
        assert_eq!(w.poll(), Some(OpencodeTurn::Working));
        // Immediately after, the throttle suppresses the read.
        assert_eq!(w.poll(), None);
        // check_now stays unthrottled.
        assert_eq!(w.check_now(), OpencodeTurn::Working);
    }

    #[test]
    fn ignores_sessions_that_predate_the_watch_or_live_elsewhere() {
        let f = fixture();
        // Finished session in the right directory but created before the
        // watch — the previous investigation run; must not match.
        insert_session(&f.conn, "ses_old", &f.worktree, 500);
        insert_message(
            &f.conn,
            "msg_old",
            "ses_old",
            600,
            r#"{"role":"assistant","time":{"created":600,"completed":700}}"#,
        );
        // Fresh session in a different directory.
        let elsewhere = f._tmp.path().join("other");
        std::fs::create_dir_all(&elsewhere).unwrap();
        insert_session(&f.conn, "ses_other", &elsewhere, 2_000);
        insert_message(
            &f.conn,
            "msg_other",
            "ses_other",
            2_100,
            r#"{"role":"assistant","time":{"created":2100,"completed":2200}}"#,
        );

        let mut w = watcher(&f, 1_000);
        assert_eq!(w.check_now(), OpencodeTurn::Working);
    }

    #[test]
    fn pending_user_prompt_and_streaming_assistant_turn_are_working() {
        let f = fixture();
        insert_session(&f.conn, "ses_1", &f.worktree, 2_000);
        insert_message(
            &f.conn,
            "msg_user",
            "ses_1",
            2_100,
            r#"{"role":"user","time":{"created":2100}}"#,
        );
        let mut w = watcher(&f, 1_000);
        assert_eq!(w.check_now(), OpencodeTurn::Working);

        // Assistant turn started, no time.completed yet.
        insert_message(
            &f.conn,
            "msg_asst",
            "ses_1",
            2_200,
            r#"{"role":"assistant","time":{"created":2200}}"#,
        );
        assert_eq!(w.check_now(), OpencodeTurn::Working);
    }

    #[test]
    fn compaction_summary_message_is_still_working() {
        let f = fixture();
        insert_session(&f.conn, "ses_1", &f.worktree, 2_000);
        insert_message(
            &f.conn,
            "msg_sum",
            "ses_1",
            2_500,
            r#"{"role":"assistant","summary":true,"time":{"created":2500,"completed":2600}}"#,
        );
        let mut w = watcher(&f, 1_000);
        assert_eq!(w.check_now(), OpencodeTurn::Working);
    }

    #[test]
    fn completed_turn_finishes_with_the_ordered_assistant_transcript() {
        let f = fixture();
        insert_session(&f.conn, "ses_1", &f.worktree, 2_000);
        insert_message(
            &f.conn,
            "msg_user",
            "ses_1",
            2_100,
            r#"{"role":"user","time":{"created":2100}}"#,
        );
        insert_part(
            &f.conn,
            "prt_user",
            "msg_user",
            "ses_1",
            r#"{"type":"text","text":"the prompt"}"#,
        );
        insert_message(
            &f.conn,
            "msg_asst",
            "ses_1",
            2_200,
            r#"{"role":"assistant","finish":"stop","time":{"created":2200,"completed":9000}}"#,
        );
        insert_part(
            &f.conn,
            "prt_1",
            "msg_asst",
            "ses_1",
            r#"{"type":"reasoning","text":"hidden thoughts"}"#,
        );
        insert_part(
            &f.conn,
            "prt_2",
            "msg_asst",
            "ses_1",
            r#"{"type":"text","text":"Investigating."}"#,
        );
        insert_part(
            &f.conn,
            "prt_3",
            "msg_asst",
            "ses_1",
            r#"{"type":"step-finish","reason":"stop"}"#,
        );
        insert_part(
            &f.conn,
            "prt_4",
            "msg_asst",
            "ses_1",
            r#"{"type":"text","text":"==== HYPOTHESIS ====\nDESCRIPTION: d\nRANKING: 3\nQUALITY: inferred\nSOLUTION: s\n==== END ===="}"#,
        );

        let mut w = watcher(&f, 1_000);
        let turn = w.check_now();
        let OpencodeTurn::Finished { transcript } = turn else {
            panic!("expected Finished, got {turn:?}");
        };
        assert_eq!(
            transcript,
            "Investigating.\n\n==== HYPOTHESIS ====\nDESCRIPTION: d\nRANKING: 3\nQUALITY: inferred\nSOLUTION: s\n==== END ===="
        );
        // The user prompt and reasoning parts never leak into the transcript.
        assert!(!transcript.contains("the prompt"));
        assert!(!transcript.contains("hidden thoughts"));
    }

    #[test]
    fn completed_turn_with_error_reports_failed() {
        let f = fixture();
        insert_session(&f.conn, "ses_1", &f.worktree, 2_000);
        insert_message(
            &f.conn,
            "msg_asst",
            "ses_1",
            2_200,
            r#"{"role":"assistant","time":{"created":2200,"completed":9000},"error":{"name":"APIError","data":{"message":"Insufficient balance"}}}"#,
        );
        let mut w = watcher(&f, 1_000);
        assert_eq!(
            w.check_now(),
            OpencodeTurn::Failed {
                message: "Insufficient balance".to_string()
            }
        );

        // Error without a data.message falls back to the error name.
        f.conn
            .execute(
                "update message set data = ?1 where id = 'msg_asst'",
                params![r#"{"role":"assistant","time":{"created":2200,"completed":9000},"error":{"name":"UnknownError"}}"#],
            )
            .unwrap();
        let mut w = watcher(&f, 1_000);
        assert_eq!(
            w.check_now(),
            OpencodeTurn::Failed {
                message: "UnknownError".to_string()
            }
        );
    }

    #[test]
    fn completed_tool_call_step_waits_for_the_terminal_assistant_message() {
        let f = fixture();
        insert_session(&f.conn, "ses_1", &f.worktree, 2_000);
        insert_message(
            &f.conn,
            "msg_tool",
            "ses_1",
            2_200,
            r#"{"role":"assistant","finish":"tool-calls","time":{"created":2200,"completed":2300}}"#,
        );
        insert_part(
            &f.conn,
            "prt_tool",
            "msg_tool",
            "ses_1",
            r#"{"type":"text","text":"Calling a tool"}"#,
        );
        let mut w = watcher(&f, 1_000);
        assert_eq!(w.check_now(), OpencodeTurn::Working);

        insert_message(
            &f.conn,
            "msg_stop",
            "ses_1",
            2_400,
            r#"{"role":"assistant","finish":"stop","time":{"created":2400,"completed":2500}}"#,
        );
        insert_part(
            &f.conn,
            "prt_stop",
            "msg_stop",
            "ses_1",
            r#"{"type":"text","text":"Final answer"}"#,
        );

        assert_eq!(
            w.check_now(),
            OpencodeTurn::Finished {
                transcript: "Calling a tool\n\nFinal answer".to_string()
            }
        );
    }

    #[test]
    fn esc_interrupt_abort_stays_working_not_failed() {
        // opencode stamps `time.completed` even on abort and tags a user
        // Esc-interrupt as `MessageAbortedError`. That is the user
        // interrupting to redirect, not a finished/failed turn: the watcher
        // must report Working so a follow-up prompt resumes the same session
        // (mirrors Codex `turn_aborted` and Claude Code interrupts) — and it
        // must never be mistaken for a green Finished.
        let f = fixture();
        insert_session(&f.conn, "ses_1", &f.worktree, 2_000);
        insert_message(
            &f.conn,
            "msg_asst",
            "ses_1",
            2_200,
            r#"{"role":"assistant","time":{"created":2200,"completed":9000},"error":{"name":"MessageAbortedError","data":{"message":"Aborted"}}}"#,
        );
        let mut w = watcher(&f, 1_000);
        assert_eq!(w.check_now(), OpencodeTurn::Working);

        // A genuine provider error at the same point is still Failed.
        f.conn
            .execute(
                "update message set data = ?1 where id = 'msg_asst'",
                params![r#"{"role":"assistant","time":{"created":2200,"completed":9000},"error":{"name":"ProviderError","data":{"message":"model unavailable"}}}"#],
            )
            .unwrap();
        let mut w = watcher(&f, 1_000);
        assert_eq!(
            w.check_now(),
            OpencodeTurn::Failed {
                message: "model unavailable".to_string()
            }
        );
    }

    #[test]
    fn session_is_pinned_once_resolved() {
        let f = fixture();
        insert_session(&f.conn, "ses_1", &f.worktree, 2_000);
        insert_message(
            &f.conn,
            "m1",
            "ses_1",
            2_100,
            r#"{"role":"assistant","time":{"created":2100}}"#,
        );
        let mut w = watcher(&f, 1_000);
        assert_eq!(w.check_now(), OpencodeTurn::Working);

        // A newer session appears in the same directory (the user opened
        // their own opencode): the watcher must stay on ses_1.
        insert_session(&f.conn, "ses_2", &f.worktree, 3_000);
        insert_message(
            &f.conn,
            "m2",
            "ses_2",
            3_100,
            r#"{"role":"assistant","time":{"created":3100,"completed":3200}}"#,
        );
        assert_eq!(w.check_now(), OpencodeTurn::Working);
        assert_eq!(w.session_id.as_deref(), Some("ses_1"));
    }

    #[test]
    fn transcript_now_reads_partial_output_before_completion() {
        let f = fixture();
        insert_session(&f.conn, "ses_1", &f.worktree, 2_000);
        insert_message(
            &f.conn,
            "msg_asst",
            "ses_1",
            2_200,
            r#"{"role":"assistant","time":{"created":2200}}"#,
        );
        insert_part(
            &f.conn,
            "prt_1",
            "msg_asst",
            "ses_1",
            r#"{"type":"text","text":"partial reply"}"#,
        );
        let mut w = watcher(&f, 1_000);
        assert_eq!(w.check_now(), OpencodeTurn::Working);
        assert_eq!(w.transcript_now().as_deref(), Some("partial reply"));
    }
}

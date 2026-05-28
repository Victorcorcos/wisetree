//! AI-harness activity detection for the dashboard.
//!
//! See `PLAN.md` for the full design rationale. In brief: each tick the
//! dashboard runs one [`AiStatusService::build_index`] inside `spawn_blocking`
//! — every enabled harness performs its global scan there — then probes the
//! resulting [`AiStatusIndex`] once per worktree with
//! [`AiStatusService::report_for`] (O(log N) lookups).

mod claude;
mod codex;
mod gemini;
mod opencode;
mod paths;
mod state;
mod util;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

pub use paths::{canonical_key, AiStatusPaths};
pub use state::{AiHarness, AiHarnessState, AiStatus, AiStatusReport};

/// What a per-harness detector returns for one tick: per-worktree states,
/// plus a flag indicating that some non-attributable failure happened (e.g.
/// a JSON file the detector couldn't parse to recover its `cwd`).
#[derive(Debug, Default)]
pub(crate) struct DetectorOutput {
    pub per_cwd: BTreeMap<PathBuf, AiHarnessState>,
    pub global_failure: bool,
}

use crate::config::schema::AiStatusConfig;

/// Per-tick global index built by every detector and consulted via
/// [`AiStatusService::report_for`].
#[derive(Debug, Clone, Default)]
pub struct AiStatusIndex {
    by_cwd: BTreeMap<PathBuf, BTreeMap<AiHarness, AiHarnessState>>,
    /// Harnesses whose scan encountered a non-attributable failure (e.g. a
    /// malformed JSON file we couldn't tie to a specific worktree). These
    /// surface as `Failed` for any worktree that has no other positive signal
    /// for that harness.
    global_failures: BTreeSet<AiHarness>,
}

impl AiStatusIndex {
    pub fn is_empty(&self) -> bool {
        self.by_cwd.is_empty() && self.global_failures.is_empty()
    }

    fn insert(&mut self, harness: AiHarness, key: PathBuf, state: AiHarnessState) {
        let entry = self.by_cwd.entry(key).or_default();
        let merged = entry
            .get(&harness)
            .copied()
            .map(|existing| AiHarnessState::merge(existing, state))
            .unwrap_or(state);
        entry.insert(harness, merged);
    }

    fn record_global_failure(&mut self, harness: AiHarness) {
        self.global_failures.insert(harness);
    }
}

/// Detection service shared across the dashboard's lifetime.
///
/// Holds the per-process configuration (enabled harnesses, active window) and
/// owns the cold-path long-tail cache that supplements the hot per-tick
/// scans for codex's flat-by-date layout.
#[derive(Debug, Clone)]
pub struct AiStatusService {
    enabled: BTreeSet<AiHarness>,
    active_window: Duration,
    paths: AiStatusPaths,
    /// Long-tail cache for codex sessions older than today/yesterday. Built
    /// lazily by `refresh_codex_cache_if_due` and merged at lookup time.
    codex_cache: Arc<RwLock<CodexLongTailCache>>,
}

#[derive(Debug, Default)]
struct CodexLongTailCache {
    /// Set of cwds that had codex activity older than today/yesterday.
    /// Presence is the entire signal — these sessions are by construction
    /// outside the active window, so they always classify as `Idle`. Earlier
    /// versions stored `SystemTime` here and ran the values through
    /// `classify_mtime`, which incorrectly flipped every long-tail entry to
    /// `Running` for `window` ms after each rebuild.
    seen_cwds: BTreeSet<PathBuf>,
    /// `None` once we've never built it; `Some(instant)` after each rebuild
    /// so we can pace work to one rebuild per minute.
    last_built: Option<Instant>,
}

const CODEX_LONG_TAIL_REBUILD_PERIOD: Duration = Duration::from_secs(60);

impl AiStatusService {
    pub fn new(config: &AiStatusConfig, paths: AiStatusPaths) -> Self {
        let enabled: BTreeSet<AiHarness> = config
            .enabled_harnesses
            .iter()
            .filter_map(|name| AiHarness::parse(name))
            .collect();
        Self {
            enabled,
            active_window: Duration::from_millis(config.active_window_ms),
            paths,
            codex_cache: Arc::new(RwLock::new(CodexLongTailCache::default())),
        }
    }

    pub fn enabled_harnesses(&self) -> &BTreeSet<AiHarness> {
        &self.enabled
    }

    /// Build the per-tick global index by running every enabled harness's
    /// global scan and merging the results. Pure I/O — call from inside
    /// `tokio::task::spawn_blocking`. Never returns Err; per-(cwd, harness)
    /// failures fold into [`AiHarnessState::Failed`].
    pub fn build_index(&self) -> AiStatusIndex {
        let mut index = AiStatusIndex::default();
        let window = self.active_window;

        if self.enabled.contains(&AiHarness::ClaudeCode) {
            let result = claude::scan(&self.paths, window);
            for (cwd, state) in result.per_cwd {
                index.insert(AiHarness::ClaudeCode, cwd, state);
            }
            if result.global_failure {
                index.record_global_failure(AiHarness::ClaudeCode);
            }
        }
        if self.enabled.contains(&AiHarness::CodexCli) {
            let result = codex::scan(&self.paths, window);
            for (cwd, state) in result.per_cwd {
                index.insert(AiHarness::CodexCli, cwd, state);
            }
            if result.global_failure {
                index.record_global_failure(AiHarness::CodexCli);
            }
            // Merge the long-tail cache so worktrees with no recent activity
            // still report `Finished` rather than `Pending`. Long-tail entries
            // are always `Idle` — they came from sessions older than
            // today/yesterday and cannot be active now.
            self.refresh_codex_cache_if_due();
            if let Ok(cache) = self.codex_cache.read() {
                for cwd in cache.seen_cwds.iter() {
                    index.insert(AiHarness::CodexCli, cwd.clone(), AiHarnessState::Idle);
                }
            }
        }
        if self.enabled.contains(&AiHarness::Opencode) {
            let result = opencode::scan(&self.paths, window);
            for (cwd, state) in result.per_cwd {
                index.insert(AiHarness::Opencode, cwd, state);
            }
            if result.global_failure {
                index.record_global_failure(AiHarness::Opencode);
            }
        }
        if self.enabled.contains(&AiHarness::GeminiCli) {
            let result = gemini::scan(&self.paths, window);
            for (cwd, state) in result.per_cwd {
                index.insert(AiHarness::GeminiCli, cwd, state);
            }
            if result.global_failure {
                index.record_global_failure(AiHarness::GeminiCli);
            }
        }

        index
    }

    /// Look up the per-worktree report. `worktree` must be the path returned
    /// by `git worktree list` (the caller does not need to canonicalize —
    /// this function does).
    pub fn report_for(&self, index: &AiStatusIndex, worktree: &Path) -> AiStatusReport {
        let key = canonical_key(worktree);
        let mut per_harness: BTreeMap<AiHarness, AiHarnessState> = self
            .enabled
            .iter()
            .copied()
            .map(|h| (h, AiHarnessState::Absent))
            .collect();

        if let Some(entries) = index.by_cwd.get(&key) {
            for (harness, state) in entries {
                if self.enabled.contains(harness) {
                    per_harness.insert(*harness, *state);
                }
            }
        }

        // Surface non-attributable scan failures: when a harness's global
        // scan choked on something we couldn't tie to a specific worktree,
        // worktrees with no positive signal for that harness display
        // `Failed` rather than silently dropping the failure.
        for harness in &index.global_failures {
            if !self.enabled.contains(harness) {
                continue;
            }
            if let Some(state) = per_harness.get_mut(harness) {
                if *state == AiHarnessState::Absent {
                    *state = AiHarnessState::Failed;
                }
            }
        }

        let aggregated = AiStatusReport::aggregate(&per_harness);
        AiStatusReport {
            aggregated,
            per_harness,
        }
    }

    fn refresh_codex_cache_if_due(&self) {
        let needs_refresh = match self.codex_cache.read() {
            Ok(cache) => cache.last_built.map_or(true, |last| {
                last.elapsed() >= CODEX_LONG_TAIL_REBUILD_PERIOD
            }),
            Err(_) => false,
        };
        if !needs_refresh {
            return;
        }
        let Some(sessions_root) = self.paths.codex_sessions.as_ref() else {
            return;
        };
        let recent: BTreeSet<PathBuf> =
            codex::recent_date_dirs(sessions_root).into_iter().collect();
        let mut by_cwd: BTreeMap<PathBuf, AiHarnessState> = BTreeMap::new();
        // Walk the year/month/day tree, skipping the days already covered by
        // the hot path.
        if let Ok(years) = std::fs::read_dir(sessions_root) {
            for year in years.flatten() {
                let year_path = year.path();
                let Ok(months) = std::fs::read_dir(&year_path) else {
                    continue;
                };
                for month in months.flatten() {
                    let month_path = month.path();
                    let Ok(days) = std::fs::read_dir(&month_path) else {
                        continue;
                    };
                    for day in days.flatten() {
                        let day_path = day.path();
                        if recent.contains(&day_path) {
                            continue;
                        }
                        codex::scan_day_dir(&day_path, self.active_window, &mut by_cwd);
                    }
                }
            }
        }
        // Collapse the per-cwd state map into a presence set. Any cwd with a
        // positive (non-Failed) signal is recorded; the merge in `build_index`
        // emits it as `Idle`. Failed entries stay out of the cache so they
        // don't override a more recent positive signal from the hot path.
        let seen_cwds: BTreeSet<PathBuf> = by_cwd
            .into_iter()
            .filter_map(|(cwd, state)| {
                matches!(state, AiHarnessState::Idle | AiHarnessState::Running).then_some(cwd)
            })
            .collect();
        if let Ok(mut cache) = self.codex_cache.write() {
            cache.seen_cwds = seen_cwds;
            cache.last_built = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::TempDir;

    fn paths_under(tmp: &TempDir) -> AiStatusPaths {
        AiStatusPaths {
            claude_projects: Some(tmp.path().join(".claude/projects")),
            claude_sessions: Some(tmp.path().join(".claude/sessions")),
            codex_sessions: Some(tmp.path().join(".codex/sessions")),
            gemini_tmp: Some(tmp.path().join(".gemini/tmp")),
            opencode_state: Some(tmp.path().join(".local/state/opencode")),
            opencode_data: Some(tmp.path().join(".local/share/opencode")),
        }
    }

    fn all_enabled_config() -> AiStatusConfig {
        AiStatusConfig::default()
    }

    fn claude_only_config(active_window_ms: u64) -> AiStatusConfig {
        AiStatusConfig {
            enabled_harnesses: vec!["claude_code".to_string()],
            active_window_ms,
        }
    }

    fn write_claude_transcript(project_dir: &std::path::Path, lines: &[String]) {
        fs::write(project_dir.join("session.jsonl"), lines.join("\n")).unwrap();
    }

    fn claude_prompt_line(cwd: &str) -> String {
        format!(r#"{{"type":"user","promptId":"prompt-1","cwd":"{cwd}"}}"#)
    }

    fn claude_assistant_line(cwd: &str, stop_reason: &str) -> String {
        // Mirror the real Claude Code v2.x transcript shape: `stop_reason`
        // lives under `message`, not at the top level. A previous version of
        // this helper inlined `stop_reason` at the top level, which matched a
        // buggy parser but never the actual transcripts on disk.
        format!(
            r#"{{"type":"assistant","cwd":"{cwd}","message":{{"role":"assistant","stop_reason":"{stop_reason}"}}}}"#
        )
    }

    fn write_claude_live_session(sessions_root: &std::path::Path, pid: u32, cwd: &str) {
        let session_json =
            format!(r#"{{"pid":{pid},"sessionId":"abc","cwd":"{cwd}","kind":"interactive"}}"#);
        fs::write(sessions_root.join(format!("{pid}.json")), session_json).unwrap();
    }

    fn write_codex_rollout(day_dir: &Path, name: &str, lines: &[String]) {
        fs::create_dir_all(day_dir).unwrap();
        fs::write(day_dir.join(name), lines.join("\n")).unwrap();
    }

    fn codex_session_meta_line(cwd: &str) -> String {
        format!(r#"{{"type":"session_meta","payload":{{"cwd":"{cwd}"}}}}"#)
    }

    fn codex_user_line(text: &str) -> String {
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
        )
    }

    fn codex_commentary_line(text: &str) -> String {
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","phase":"commentary","content":[{{"type":"output_text","text":"{text}"}}]}}}}"#
        )
    }

    fn codex_final_line(text: &str) -> String {
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","phase":"final_answer","content":[{{"type":"output_text","text":"{text}"}}]}}}}"#
        )
    }

    fn codex_task_started_line() -> String {
        r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t-1"}}"#.to_string()
    }

    fn codex_task_complete_line() -> String {
        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t-1"}}"#.to_string()
    }

    fn codex_turn_aborted_line() -> String {
        r#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t-1"}}"#.to_string()
    }

    fn write_gemini_project_root(project_dir: &Path, worktree: &Path) {
        fs::create_dir_all(project_dir.join("chats")).unwrap();
        fs::write(
            project_dir.join(".project_root"),
            worktree.to_string_lossy().as_bytes(),
        )
        .unwrap();
    }

    fn write_gemini_chat(project_dir: &Path, name: &str, lines: &[String]) {
        fs::create_dir_all(project_dir.join("chats")).unwrap();
        fs::write(project_dir.join("chats").join(name), lines.join("\n")).unwrap();
    }

    fn gemini_session_header_line() -> String {
        r#"{"sessionId":"session-1","kind":"main"}"#.to_string()
    }

    fn gemini_user_line(text: &str) -> String {
        format!(r#"{{"type":"user","content":[{{"text":"{text}"}}]}}"#)
    }

    fn gemini_thinking_line() -> String {
        r#"{"type":"gemini","content":"","thoughts":[{"subject":"Thinking","description":"Working"}]}"#
            .to_string()
    }

    fn gemini_final_line(text: &str) -> String {
        format!(r#"{{"type":"gemini","content":"{text}","thoughts":[]}}"#)
    }

    fn create_opencode_db(data_dir: &Path) -> rusqlite::Connection {
        fs::create_dir_all(data_dir).unwrap();
        let conn = rusqlite::Connection::open(data_dir.join("opencode.db")).unwrap();
        conn.execute(
            "create table session (
                id text primary key,
                directory text not null,
                time_updated integer not null
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "create table message (
                id text primary key,
                session_id text not null,
                time_created integer not null,
                time_updated integer not null,
                data text not null
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "create table part (
                id text primary key,
                message_id text not null,
                session_id text not null,
                time_created integer not null,
                time_updated integer not null,
                data text not null
            )",
            [],
        )
        .unwrap();
        conn
    }

    fn insert_opencode_session(
        conn: &rusqlite::Connection,
        session_id: &str,
        worktree: &Path,
        time_updated_ms: i64,
    ) {
        conn.execute(
            "insert into session (id, directory, time_updated) values (?1, ?2, ?3)",
            rusqlite::params![session_id, worktree.to_string_lossy(), time_updated_ms],
        )
        .unwrap();
    }

    fn insert_opencode_message(
        conn: &rusqlite::Connection,
        id: &str,
        session_id: &str,
        time_created_ms: i64,
        time_updated_ms: i64,
        data: &str,
    ) {
        conn.execute(
            "insert into message (id, session_id, time_created, time_updated, data)
             values (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, session_id, time_created_ms, time_updated_ms, data],
        )
        .unwrap();
    }

    fn insert_opencode_part(
        conn: &rusqlite::Connection,
        id: &str,
        message_id: &str,
        session_id: &str,
        time_created_ms: i64,
        time_updated_ms: i64,
        data: &str,
    ) {
        conn.execute(
            "insert into part (id, message_id, session_id, time_created, time_updated, data)
             values (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                id,
                message_id,
                session_id,
                time_created_ms,
                time_updated_ms,
                data
            ],
        )
        .unwrap();
    }

    #[test]
    fn empty_layout_reports_none() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = AiStatusService::new(&all_enabled_config(), paths_under(&tmp));
        let index = svc.build_index();
        let report = svc.report_for(&index, &tmp.path().join("project"));
        assert_eq!(report.aggregated, AiStatus::None);
        for state in report.per_harness.values() {
            assert_eq!(*state, AiHarnessState::Absent);
        }
    }

    #[test]
    fn aggregate_priority_running_beats_idle_beats_failed() {
        let mut map: BTreeMap<AiHarness, AiHarnessState> = BTreeMap::new();
        map.insert(AiHarness::ClaudeCode, AiHarnessState::Failed);
        map.insert(AiHarness::Opencode, AiHarnessState::Idle);
        assert_eq!(AiStatusReport::aggregate(&map), AiStatus::Finished);
        map.insert(AiHarness::CodexCli, AiHarnessState::Running);
        assert_eq!(AiStatusReport::aggregate(&map), AiStatus::InProgress);
    }

    #[test]
    fn aggregate_failed_only_when_no_positive_signal() {
        let mut map: BTreeMap<AiHarness, AiHarnessState> = BTreeMap::new();
        map.insert(AiHarness::ClaudeCode, AiHarnessState::Failed);
        map.insert(AiHarness::Opencode, AiHarnessState::Absent);
        assert_eq!(AiStatusReport::aggregate(&map), AiStatus::Failed);
    }

    #[test]
    fn opencode_session_diff_with_recent_mtime_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let session_dir = paths
            .opencode_data
            .as_ref()
            .unwrap()
            .join("storage/session_diff");
        fs::create_dir_all(&session_dir).unwrap();
        let json = format!(r#"{{"cwd":"{cwd_str}"}}"#);
        fs::write(session_dir.join("ses_abc.json"), json).unwrap();

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::InProgress);
        assert_eq!(
            report.per_harness.get(&AiHarness::Opencode),
            Some(&AiHarnessState::Running)
        );
    }

    #[test]
    fn opencode_database_thinking_uses_latest_part_not_stale_session_time() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let data_dir = paths.opencode_data.as_ref().unwrap();
        let conn = create_opencode_db(data_dir);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let stale_session_ms = now_ms - 60_000;
        insert_opencode_session(&conn, "ses_current", &worktree, stale_session_ms);
        insert_opencode_message(
            &conn,
            "msg_user",
            "ses_current",
            stale_session_ms,
            stale_session_ms,
            r#"{"role":"user"}"#,
        );
        insert_opencode_message(
            &conn,
            "msg_assistant",
            "ses_current",
            stale_session_ms + 1,
            stale_session_ms + 1,
            r#"{"role":"assistant"}"#,
        );
        insert_opencode_part(
            &conn,
            "prt_reasoning",
            "msg_assistant",
            "ses_current",
            now_ms - 1,
            now_ms,
            r#"{"type":"reasoning","text":"still thinking"}"#,
        );

        let cfg = AiStatusConfig {
            enabled_harnesses: vec!["opencode".to_string()],
            ..AiStatusConfig::default()
        };
        let svc = AiStatusService::new(&cfg, paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::InProgress);
        assert_eq!(
            report.per_harness.get(&AiHarness::Opencode),
            Some(&AiHarnessState::Running)
        );
    }

    #[test]
    fn opencode_database_final_stop_marks_finished_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let data_dir = paths.opencode_data.as_ref().unwrap();
        let conn = create_opencode_db(data_dir);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        insert_opencode_session(&conn, "ses_current", &worktree, now_ms);
        insert_opencode_message(
            &conn,
            "msg_user",
            "ses_current",
            now_ms - 2,
            now_ms - 2,
            r#"{"role":"user"}"#,
        );
        insert_opencode_message(
            &conn,
            "msg_assistant",
            "ses_current",
            now_ms - 1,
            now_ms - 1,
            r#"{"role":"assistant"}"#,
        );
        insert_opencode_part(
            &conn,
            "prt_stop",
            "msg_assistant",
            "ses_current",
            now_ms,
            now_ms,
            r#"{"type":"step-finish","reason":"stop"}"#,
        );

        let cfg = AiStatusConfig {
            enabled_harnesses: vec!["opencode".to_string()],
            ..AiStatusConfig::default()
        };
        let svc = AiStatusService::new(&cfg, paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::Finished);
        assert_eq!(
            report.per_harness.get(&AiHarness::Opencode),
            Some(&AiHarnessState::Idle)
        );
    }

    #[test]
    fn opencode_session_diff_array_without_cwd_is_not_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let session_dir = paths
            .opencode_data
            .as_ref()
            .unwrap()
            .join("storage/session_diff");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("ses_current.json"),
            r#"[{"file":"src/main.rs","status":"modified"}]"#,
        )
        .unwrap();

        let cfg = AiStatusConfig {
            enabled_harnesses: vec!["opencode".to_string()],
            ..AiStatusConfig::default()
        };
        let svc = AiStatusService::new(&cfg, paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &tmp.path().join("project"));
        assert_eq!(report.aggregated, AiStatus::None);
        assert_eq!(
            report.per_harness.get(&AiHarness::Opencode),
            Some(&AiHarnessState::Absent)
        );
    }

    #[test]
    fn claude_recent_unresolved_prompt_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let project_dir = paths.claude_projects.as_ref().unwrap().join("dash-slug");
        fs::create_dir_all(&project_dir).unwrap();
        write_claude_transcript(&project_dir, &[claude_prompt_line(&cwd_str)]);

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::InProgress);
        assert_eq!(
            report.per_harness.get(&AiHarness::ClaudeCode),
            Some(&AiHarnessState::Running)
        );
    }

    #[test]
    fn claude_live_session_file_running_even_when_jsonl_is_stale() {
        // Regression: if the latest transcript turn is still unresolved but
        // the JSONL has gone stale during a long tool call, a live Claude
        // session PID should keep the worktree in Running.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let project_dir = paths.claude_projects.as_ref().unwrap().join("dash-slug");
        fs::create_dir_all(&project_dir).unwrap();
        write_claude_transcript(&project_dir, &[claude_prompt_line(&cwd_str)]);
        std::thread::sleep(std::time::Duration::from_millis(25));
        let sessions_root = paths.claude_sessions.as_ref().unwrap();
        fs::create_dir_all(sessions_root).unwrap();
        let live_pid = std::process::id();
        write_claude_live_session(sessions_root, live_pid, &cwd_str);

        let svc = AiStatusService::new(&claude_only_config(1), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::InProgress);
        assert_eq!(
            report.per_harness.get(&AiHarness::ClaudeCode),
            Some(&AiHarnessState::Running)
        );
    }

    #[test]
    fn claude_tool_use_with_recent_mtime_runs() {
        // Real Claude Code v2.x writes `message.stop_reason: "tool_use"` while
        // the assistant is waiting on a tool call. The latest assistant line
        // in that case is NOT a finished turn — the worktree must stay
        // Running, not flip to Idle/Finished.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let project_dir = paths.claude_projects.as_ref().unwrap().join("dash-slug");
        fs::create_dir_all(&project_dir).unwrap();
        write_claude_transcript(
            &project_dir,
            &[
                claude_prompt_line(&cwd_str),
                claude_assistant_line(&cwd_str, "tool_use"),
            ],
        );

        let svc = AiStatusService::new(&claude_only_config(10_000), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::InProgress);
        assert_eq!(
            report.per_harness.get(&AiHarness::ClaudeCode),
            Some(&AiHarnessState::Running)
        );
    }

    #[test]
    fn claude_completed_turn_with_live_session_is_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let project_dir = paths.claude_projects.as_ref().unwrap().join("dash-slug");
        fs::create_dir_all(&project_dir).unwrap();
        write_claude_transcript(
            &project_dir,
            &[
                claude_prompt_line(&cwd_str),
                claude_assistant_line(&cwd_str, "end_turn"),
            ],
        );
        let sessions_root = paths.claude_sessions.as_ref().unwrap();
        fs::create_dir_all(sessions_root).unwrap();
        write_claude_live_session(sessions_root, std::process::id(), &cwd_str);

        let svc = AiStatusService::new(&claude_only_config(1), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::Finished);
        assert_eq!(
            report.per_harness.get(&AiHarness::ClaudeCode),
            Some(&AiHarnessState::Idle)
        );
    }

    #[test]
    fn claude_stale_unresolved_prompt_without_live_session_is_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let project_dir = paths.claude_projects.as_ref().unwrap().join("dash-slug");
        fs::create_dir_all(&project_dir).unwrap();
        write_claude_transcript(&project_dir, &[claude_prompt_line(&cwd_str)]);
        std::thread::sleep(std::time::Duration::from_millis(25));

        let svc = AiStatusService::new(&claude_only_config(1), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::Finished);
        assert_eq!(
            report.per_harness.get(&AiHarness::ClaudeCode),
            Some(&AiHarnessState::Idle)
        );
    }

    #[test]
    fn claude_stale_session_file_with_dead_pid_is_ignored() {
        // PID 0 is reserved by the kernel; `kill(0, 0)` returns EPERM-or-success
        // depending on the OS, so we use a guaranteed-dead PID from the
        // documented "unlikely to be assigned" range. The test asserts that
        // a session file whose PID isn't alive does not flip the worktree to
        // Running — otherwise a crashed-and-not-cleaned-up session would
        // mark the worktree as active forever.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.claude_sessions.as_ref().unwrap();
        fs::create_dir_all(sessions_root).unwrap();
        // 0x7FFFFFFE — well beyond any realistic OS PID — gives ESRCH on both
        // macOS (PID_MAX 99999) and Linux (default 32768, max 4194304).
        let dead_pid: u32 = 0x7FFF_FFFE;
        write_claude_live_session(sessions_root, dead_pid, &cwd_str);

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(
            report.per_harness.get(&AiHarness::ClaudeCode),
            Some(&AiHarnessState::Absent)
        );
    }

    #[test]
    fn gemini_unresolved_prompt_with_recent_activity_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("ai-status");
        fs::create_dir_all(&worktree).unwrap();
        let project_dir = paths.gemini_tmp.as_ref().unwrap().join("ai-status");
        write_gemini_project_root(&project_dir, &worktree);
        write_gemini_chat(
            &project_dir,
            "session.jsonl",
            &[
                gemini_session_header_line(),
                gemini_user_line("fix the footer"),
                gemini_thinking_line(),
            ],
        );

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::InProgress);
        assert_eq!(
            report.per_harness.get(&AiHarness::GeminiCli),
            Some(&AiHarnessState::Running)
        );
    }

    #[test]
    fn gemini_non_empty_response_marks_finished_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("ai-status");
        fs::create_dir_all(&worktree).unwrap();
        let project_dir = paths.gemini_tmp.as_ref().unwrap().join("ai-status");
        write_gemini_project_root(&project_dir, &worktree);
        write_gemini_chat(
            &project_dir,
            "session.jsonl",
            &[
                gemini_session_header_line(),
                gemini_user_line("fix the footer"),
                gemini_final_line("Done."),
            ],
        );

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::Finished);
        assert_eq!(
            report.per_harness.get(&AiHarness::GeminiCli),
            Some(&AiHarnessState::Idle)
        );
    }

    #[test]
    fn gemini_stale_unresolved_prompt_without_reply_is_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("ai-status");
        fs::create_dir_all(&worktree).unwrap();
        let project_dir = paths.gemini_tmp.as_ref().unwrap().join("ai-status");
        write_gemini_project_root(&project_dir, &worktree);
        write_gemini_chat(
            &project_dir,
            "session.jsonl",
            &[
                gemini_session_header_line(),
                gemini_user_line("fix the footer"),
            ],
        );
        std::thread::sleep(std::time::Duration::from_millis(25));

        let cfg = AiStatusConfig {
            enabled_harnesses: vec!["gemini_cli".to_string()],
            active_window_ms: 1,
        };
        let svc = AiStatusService::new(&cfg, paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::Finished);
        assert_eq!(
            report.per_harness.get(&AiHarness::GeminiCli),
            Some(&AiHarnessState::Idle)
        );
    }

    #[test]
    fn gemini_stale_pending_with_live_process_runs() {
        // Regression: gemini-cli stops writing to its JSONL while a long
        // "Thinking…" step is in flight (we've observed gaps of several
        // minutes on slow models). The mtime-only fallback then ages past
        // `active_window_ms` and flips the worktree to Idle even though the
        // process is alive. A live `gemini` cwd must override that.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("ai-status");
        fs::create_dir_all(&worktree).unwrap();
        let project_dir = paths.gemini_tmp.as_ref().unwrap().join("ai-status");
        write_gemini_project_root(&project_dir, &worktree);
        write_gemini_chat(
            &project_dir,
            "session.jsonl",
            &[
                gemini_session_header_line(),
                gemini_user_line("investigate this repo"),
            ],
        );
        std::thread::sleep(std::time::Duration::from_millis(25));

        let mut live_cwds = std::collections::BTreeSet::new();
        live_cwds.insert(canonical_key(&worktree));

        let out = gemini::scan_with_live_cwds_for_test(
            &paths,
            std::time::Duration::from_millis(1),
            &live_cwds,
        );
        let state = out.per_cwd.get(&canonical_key(&worktree)).copied();
        assert_eq!(state, Some(AiHarnessState::Running));
    }

    #[test]
    fn gemini_legacy_hash_dir_without_project_root_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        // Simulate an old gemini-cli layout: sha256-style dir name, no
        // `.project_root` file. We can't attribute these to a worktree, so
        // they must not flip any cwd to a positive signal.
        let project_dir = paths
            .gemini_tmp
            .as_ref()
            .unwrap()
            .join("a82356f93285589653d2c37a1f993603bf7335d0c35341f4396eae079aa2227c");
        fs::create_dir_all(project_dir.join("chats")).unwrap();
        fs::write(
            project_dir.join("chats/session.jsonl"),
            gemini_session_header_line(),
        )
        .unwrap();

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(
            report.per_harness.get(&AiHarness::GeminiCli),
            Some(&AiHarnessState::Absent)
        );
    }

    #[test]
    fn codex_long_tail_cache_emits_idle_not_running() {
        // Regression: the long-tail cache used to store `SystemTime::now()`
        // as the recorded mtime for every cwd it found, then run that through
        // `classify_mtime`, which incorrectly flipped every long-tail entry
        // to `Running` for the first `window` ms after each rebuild. The
        // long-tail by construction holds sessions older than today/yesterday,
        // so they must surface as `Idle` (= aggregate `Finished`).
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("old-project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.codex_sessions.as_ref().unwrap().clone();
        // Place the session two weeks back so neither today nor yesterday
        // covers it; only the long-tail cache can surface it.
        let two_weeks_ago = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 14);
        let old_day = codex::date_dir_for(&sessions_root, two_weeks_ago).unwrap();
        write_codex_rollout(
            &old_day,
            "rollout-old.jsonl",
            &[
                codex_session_meta_line(&cwd_str),
                codex_user_line("hello"),
                codex_final_line("done"),
            ],
        );

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::Finished);
        assert_eq!(
            report.per_harness.get(&AiHarness::CodexCli),
            Some(&AiHarnessState::Idle)
        );
    }

    #[test]
    fn codex_unresolved_prompt_with_commentary_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.codex_sessions.as_ref().unwrap().clone();
        let today = codex::date_dir_for(&sessions_root, SystemTime::now()).unwrap();
        write_codex_rollout(
            &today,
            "rollout-1.jsonl",
            &[
                codex_session_meta_line(&cwd_str),
                codex_user_line("hello"),
                codex_commentary_line("checking the code"),
            ],
        );

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::InProgress);
        assert_eq!(
            report.per_harness.get(&AiHarness::CodexCli),
            Some(&AiHarnessState::Running)
        );
    }

    #[test]
    fn codex_final_answer_marks_finished_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.codex_sessions.as_ref().unwrap().clone();
        let today = codex::date_dir_for(&sessions_root, SystemTime::now()).unwrap();
        write_codex_rollout(
            &today,
            "rollout-1.jsonl",
            &[
                codex_session_meta_line(&cwd_str),
                codex_user_line("hello"),
                codex_final_line("done"),
            ],
        );

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::Finished);
        assert_eq!(
            report.per_harness.get(&AiHarness::CodexCli),
            Some(&AiHarnessState::Idle)
        );
    }

    #[test]
    fn codex_newer_finished_rollout_beats_older_running_rollout() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.codex_sessions.as_ref().unwrap().clone();
        let yesterday = codex::date_dir_for(
            &sessions_root,
            SystemTime::now() - Duration::from_secs(86_400),
        )
        .unwrap();
        let today = codex::date_dir_for(&sessions_root, SystemTime::now()).unwrap();
        write_codex_rollout(
            &yesterday,
            "rollout-yesterday.jsonl",
            &[
                codex_session_meta_line(&cwd_str),
                codex_user_line("older request"),
                codex_commentary_line("still thinking"),
            ],
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
        write_codex_rollout(
            &today,
            "rollout-today.jsonl",
            &[
                codex_session_meta_line(&cwd_str),
                codex_user_line("newer request"),
                codex_final_line("done"),
            ],
        );

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::Finished);
        assert_eq!(
            report.per_harness.get(&AiHarness::CodexCli),
            Some(&AiHarnessState::Idle)
        );
    }

    #[test]
    fn codex_session_meta_only_is_idle_not_running() {
        // Regression for CODEX_IMPROVEMENT.md: a freshly-opened codex window
        // writes the `session_meta` header immediately, giving the rollout
        // file a fresh mtime. The user hasn't typed yet — no `task_started`
        // event, no `response_item` user line — so the harness is sitting at
        // the prompt waiting for input. That must surface as `Idle`. The
        // earlier code fell through to `classify_mtime` here and incorrectly
        // showed `Running` for `active_window_ms` after every codex launch.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.codex_sessions.as_ref().unwrap().clone();
        let today = codex::date_dir_for(&sessions_root, SystemTime::now()).unwrap();
        write_codex_rollout(
            &today,
            "rollout-fresh.jsonl",
            &[codex_session_meta_line(&cwd_str)],
        );

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::Finished);
        assert_eq!(
            report.per_harness.get(&AiHarness::CodexCli),
            Some(&AiHarnessState::Idle)
        );
    }

    #[test]
    fn codex_task_started_without_complete_is_running() {
        // Authoritative path: `event_msg` with `task_started` payload marks
        // the start of a turn. If `task_complete`/`turn_aborted` hasn't
        // appeared yet, the worktree is `Running` regardless of which
        // `response_item` messages were emitted in between.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.codex_sessions.as_ref().unwrap().clone();
        let today = codex::date_dir_for(&sessions_root, SystemTime::now()).unwrap();
        write_codex_rollout(
            &today,
            "rollout-1.jsonl",
            &[codex_session_meta_line(&cwd_str), codex_task_started_line()],
        );

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::InProgress);
        assert_eq!(
            report.per_harness.get(&AiHarness::CodexCli),
            Some(&AiHarnessState::Running)
        );
    }

    #[test]
    fn codex_task_complete_marks_idle_even_with_fresh_mtime() {
        // `task_complete` after `task_started` ends the turn. The file mtime
        // is fresh (just written) but the harness is back at the prompt — so
        // `Idle`, not `Running`.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.codex_sessions.as_ref().unwrap().clone();
        let today = codex::date_dir_for(&sessions_root, SystemTime::now()).unwrap();
        write_codex_rollout(
            &today,
            "rollout-1.jsonl",
            &[
                codex_session_meta_line(&cwd_str),
                codex_task_started_line(),
                codex_task_complete_line(),
            ],
        );

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::Finished);
        assert_eq!(
            report.per_harness.get(&AiHarness::CodexCli),
            Some(&AiHarnessState::Idle)
        );
    }

    #[test]
    fn codex_turn_aborted_marks_idle() {
        // `turn_aborted` is a terminal lifecycle event (Ctrl-C or guardrail
        // abort). Same effect as `task_complete`: turn over, harness idle.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.codex_sessions.as_ref().unwrap().clone();
        let today = codex::date_dir_for(&sessions_root, SystemTime::now()).unwrap();
        write_codex_rollout(
            &today,
            "rollout-1.jsonl",
            &[
                codex_session_meta_line(&cwd_str),
                codex_task_started_line(),
                codex_turn_aborted_line(),
            ],
        );

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::Finished);
        assert_eq!(
            report.per_harness.get(&AiHarness::CodexCli),
            Some(&AiHarnessState::Idle)
        );
    }

    #[test]
    fn codex_stale_pending_with_live_process_runs() {
        // Long shell/tool calls can pause JSONL writes for minutes — the
        // mtime ages past `active_window_ms` even though codex is still
        // chewing on the turn. A live `codex` process at this cwd must keep
        // the worktree `Running`. Mirrors `gemini_stale_pending_with_live_process_runs`.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.codex_sessions.as_ref().unwrap().clone();
        let today = codex::date_dir_for(&sessions_root, SystemTime::now()).unwrap();
        write_codex_rollout(
            &today,
            "rollout-1.jsonl",
            &[codex_session_meta_line(&cwd_str), codex_task_started_line()],
        );
        std::thread::sleep(std::time::Duration::from_millis(25));

        let mut live_cwds = BTreeSet::new();
        live_cwds.insert(canonical_key(&worktree));

        let out = codex::scan_with_live_cwds_for_test(
            &paths,
            std::time::Duration::from_millis(1),
            &live_cwds,
        );
        let state = out.per_cwd.get(&canonical_key(&worktree)).copied();
        assert_eq!(state, Some(AiHarnessState::Running));
    }

    #[test]
    fn codex_completed_with_live_process_is_idle_at_prompt() {
        // Even when the codex process is still alive at this cwd, a closed
        // turn (`task_complete`) means the user is back at the prompt. The
        // harness is `Idle`, not `Running`.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.codex_sessions.as_ref().unwrap().clone();
        let today = codex::date_dir_for(&sessions_root, SystemTime::now()).unwrap();
        write_codex_rollout(
            &today,
            "rollout-1.jsonl",
            &[
                codex_session_meta_line(&cwd_str),
                codex_task_started_line(),
                codex_task_complete_line(),
            ],
        );

        let mut live_cwds = BTreeSet::new();
        live_cwds.insert(canonical_key(&worktree));

        let out = codex::scan_with_live_cwds_for_test(
            &paths,
            std::time::Duration::from_millis(10_000),
            &live_cwds,
        );
        let state = out.per_cwd.get(&canonical_key(&worktree)).copied();
        assert_eq!(state, Some(AiHarnessState::Idle));
    }

    #[test]
    fn codex_stale_pending_without_live_process_is_idle() {
        // Without a live codex process AND with a stale mtime, a Pending
        // transcript was likely abandoned (process crashed, terminal closed).
        // Surface as `Idle` — there's no evidence anyone is actively working.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.codex_sessions.as_ref().unwrap().clone();
        let today = codex::date_dir_for(&sessions_root, SystemTime::now()).unwrap();
        write_codex_rollout(
            &today,
            "rollout-1.jsonl",
            &[codex_session_meta_line(&cwd_str), codex_task_started_line()],
        );
        std::thread::sleep(std::time::Duration::from_millis(25));

        let out = codex::scan_with_live_cwds_for_test(
            &paths,
            std::time::Duration::from_millis(1),
            &BTreeSet::new(),
        );
        let state = out.per_cwd.get(&canonical_key(&worktree)).copied();
        assert_eq!(state, Some(AiHarnessState::Idle));
    }

    #[test]
    fn malformed_opencode_json_reports_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let session_dir = paths
            .opencode_data
            .as_ref()
            .unwrap()
            .join("storage/session_diff");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("ses_bad.json"), "not json").unwrap();
        // No other harness has any signal, so the aggregate should be Failed.
        let cfg = AiStatusConfig {
            enabled_harnesses: vec!["opencode".to_string()],
            ..AiStatusConfig::default()
        };
        let svc = AiStatusService::new(&cfg, paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &tmp.path().join("unrelated"));
        assert_eq!(report.aggregated, AiStatus::Failed);
    }

    #[test]
    fn path_canonicalization_strips_trailing_separator() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let with_trailing = worktree.join("");
        assert_eq!(canonical_key(&worktree), canonical_key(&with_trailing));
    }

    #[test]
    fn disabled_harness_omitted_from_report() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = AiStatusConfig {
            enabled_harnesses: vec!["claude_code".to_string()],
            ..AiStatusConfig::default()
        };
        let svc = AiStatusService::new(&cfg, paths_under(&tmp));
        let index = svc.build_index();
        let report = svc.report_for(&index, &tmp.path().join("project"));
        assert!(report.per_harness.contains_key(&AiHarness::ClaudeCode));
        assert!(!report.per_harness.contains_key(&AiHarness::Opencode));
        assert!(!report.per_harness.contains_key(&AiHarness::CodexCli));
        assert!(!report.per_harness.contains_key(&AiHarness::GeminiCli));
    }
}

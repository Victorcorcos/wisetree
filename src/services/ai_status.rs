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
    fn opencode_database_session_with_recent_time_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let data_dir = paths.opencode_data.as_ref().unwrap();
        fs::create_dir_all(data_dir).unwrap();
        let conn = rusqlite::Connection::open(data_dir.join("opencode.db")).unwrap();
        conn.execute(
            "create table session (id text primary key, directory text not null, time_updated integer not null)",
            [],
        )
        .unwrap();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        conn.execute(
            "insert into session (id, directory, time_updated) values (?1, ?2, ?3)",
            rusqlite::params!["ses_current", worktree.to_string_lossy(), now_ms],
        )
        .unwrap();
        let session_dir = data_dir.join("storage/session_diff");
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
        let report = svc.report_for(&index, &worktree);
        assert_eq!(report.aggregated, AiStatus::InProgress);
        assert_eq!(
            report.per_harness.get(&AiHarness::Opencode),
            Some(&AiHarnessState::Running)
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
    fn claude_jsonl_classified_by_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let project_dir = paths.claude_projects.as_ref().unwrap().join("dash-slug");
        fs::create_dir_all(&project_dir).unwrap();
        let line = format!(r#"{{"type":"user","cwd":"{cwd_str}"}}"#);
        fs::write(project_dir.join("session.jsonl"), line).unwrap();

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
        // Regression: when Claude Code is busy on a long tool call (e.g. a
        // sub-agent), the project JSONL stops being written for many minutes
        // and the old mtime-based detector would flip the worktree from
        // Running to Idle. The `~/.claude/sessions/<pid>.json` file is the
        // authoritative "currently running" signal — its presence plus a
        // live PID means the session is active.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.claude_sessions.as_ref().unwrap();
        fs::create_dir_all(sessions_root).unwrap();
        let live_pid = std::process::id();
        let session_json = format!(
            r#"{{"pid":{live_pid},"sessionId":"abc","cwd":"{cwd_str}","kind":"interactive"}}"#
        );
        fs::write(
            sessions_root.join(format!("{live_pid}.json")),
            session_json,
        )
        .unwrap();

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
        let session_json = format!(
            r#"{{"pid":{dead_pid},"sessionId":"abc","cwd":"{cwd_str}","kind":"interactive"}}"#
        );
        fs::write(
            sessions_root.join(format!("{dead_pid}.json")),
            session_json,
        )
        .unwrap();

        let svc = AiStatusService::new(&all_enabled_config(), paths);
        let index = svc.build_index();
        let report = svc.report_for(&index, &worktree);
        assert_eq!(
            report.per_harness.get(&AiHarness::ClaudeCode),
            Some(&AiHarnessState::Absent)
        );
    }

    #[test]
    fn gemini_basename_dir_with_project_root_running() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("ai-status");
        fs::create_dir_all(&worktree).unwrap();
        let project_dir = paths.gemini_tmp.as_ref().unwrap().join("ai-status");
        fs::create_dir_all(project_dir.join("chats")).unwrap();
        fs::write(
            project_dir.join(".project_root"),
            worktree.to_string_lossy().as_bytes(),
        )
        .unwrap();
        fs::write(project_dir.join("chats/session.jsonl"), "{}").unwrap();

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
        fs::write(project_dir.join("chats/session.jsonl"), "{}").unwrap();

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
        fs::create_dir_all(&old_day).unwrap();
        let header = format!(r#"{{"cwd":"{cwd_str}","type":"session_meta"}}"#);
        fs::write(old_day.join("rollout-old.jsonl"), header).unwrap();

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
    fn codex_today_directory_running() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_under(&tmp);
        let worktree = tmp.path().join("project");
        fs::create_dir_all(&worktree).unwrap();
        let cwd_str = worktree.to_string_lossy().to_string();
        let sessions_root = paths.codex_sessions.as_ref().unwrap().clone();
        let today = codex::date_dir_for(&sessions_root, SystemTime::now()).unwrap();
        fs::create_dir_all(&today).unwrap();
        let header = format!(r#"{{"cwd":"{cwd_str}","type":"session_meta"}}"#);
        fs::write(today.join("rollout-1.jsonl"), header).unwrap();

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

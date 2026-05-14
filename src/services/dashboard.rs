//! Live dashboard polling service.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::{self, MissedTickBehavior};

use crate::config::schema::{normalize_dashboard_columns, DashboardConfig};
use crate::constants::dashboard_pr_cache_file;
use crate::errors::{handle_git_error, Result, WisetreeError};
use crate::git::exec::execute_git_command;
use crate::git::types::{BranchStatus, GitWorktree};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
/// `gh api graphql` may include the network round-trip — give it more headroom
/// than local git calls.
const GH_GRAPHQL_TIMEOUT: Duration = Duration::from_secs(8);
/// How long a cached PR record stays fresh when the branch HEAD hasn't moved.
/// Catches remote-only changes (merge, close, title edit) without hammering
/// the API.
const PR_CACHE_TTL_MS: u64 = 30 * 1000;
/// How long to suspend PR fetches after a rate-limit error.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSummary {
    pub sha: String,
    pub summary: String,
    #[serde(rename = "relativeTime")]
    pub relative_time: String,
    pub author: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrState {
    Open,
    Merged,
    Closed,
    Draft,
}

/// Aggregated CI status for the PR's most recent commit. Populated from the
/// GitHub Checks API and the legacy commit-status API so providers like
/// Drone CI (status contexts) and GitHub Actions (check runs) both feed the
/// same field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckStatus {
    Pending,
    Running,
    Passed,
    Failed,
    Errored,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub state: PrState,
    pub url: String,
    pub title: String,
    #[serde(
        rename = "checksStatus",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub checks_status: Option<CheckStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardRow {
    #[serde(flatten)]
    pub worktree: GitWorktree,
    #[serde(rename = "lastCommit", skip_serializing_if = "Option::is_none")]
    pub last_commit: Option<CommitSummary>,
    #[serde(rename = "pullRequest", skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardNoticeLevel {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardNotice {
    pub level: DashboardNoticeLevel,
    pub message: String,
}

impl DashboardNotice {
    fn warning(message: impl Into<String>) -> Self {
        Self {
            level: DashboardNoticeLevel::Warning,
            message: message.into(),
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            level: DashboardNoticeLevel::Error,
            message: message.into(),
        }
    }
}

/// Discriminates the two row emissions per refresh cycle so the UI can
/// tell apart git-only data from gh-enriched data (PR state + CI checks).
#[derive(Debug)]
pub enum DashboardUpdate {
    GitOnly(Vec<DashboardRow>),
    WithPRs(Vec<DashboardRow>),
}

impl DashboardUpdate {
    pub fn rows(&self) -> &Vec<DashboardRow> {
        match self {
            Self::GitOnly(rows) | Self::WithPRs(rows) => rows,
        }
    }

    pub fn into_rows(self) -> Vec<DashboardRow> {
        match self {
            Self::GitOnly(rows) | Self::WithPRs(rows) => rows,
        }
    }
}

#[derive(Debug)]
pub struct DashboardWatch {
    pub rx: mpsc::Receiver<DashboardUpdate>,
    pub notice_rx: mpsc::Receiver<DashboardNotice>,
    cancel: Option<oneshot::Sender<()>>,
    refresh_tx: mpsc::Sender<()>,
}

impl DashboardWatch {
    pub fn refresh(&self) {
        let _ = self.refresh_tx.try_send(());
    }
}

impl Drop for DashboardWatch {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PrCacheEntry {
    sha: String,
    #[serde(rename = "fetchedAtMs")]
    fetched_at_ms: u64,
    #[serde(rename = "pullRequest")]
    pull_request: Option<PullRequest>,
}

/// On-disk schema: `repo_root` → `branch` → entry.
type DiskCache = HashMap<String, HashMap<String, PrCacheEntry>>;

#[derive(Debug, Default)]
struct PrCacheState {
    entries: HashMap<String, PrCacheEntry>,
    repo_slug: Option<(String, String)>,
    rate_limited_until: Option<Instant>,
    rate_limit_notice_sent: bool,
    loaded_from_disk: bool,
    notice_tx: Option<mpsc::Sender<DashboardNotice>>,
}

#[derive(Debug, Clone)]
pub struct DashboardService {
    git_root: PathBuf,
    config: DashboardConfig,
    gh_available: bool,
    git_binary: PathBuf,
    gh_binary: PathBuf,
    cache_path: Option<PathBuf>,
    pr_state: Arc<Mutex<PrCacheState>>,
}

impl DashboardService {
    pub fn new(git_root: PathBuf, mut config: DashboardConfig) -> Self {
        config.clamp();
        let git_binary = PathBuf::from("git");
        let gh_binary = PathBuf::from("gh");
        let gh_available = binary_available(&gh_binary);
        Self {
            git_root,
            config,
            gh_available,
            git_binary,
            gh_binary,
            cache_path: Some(dashboard_pr_cache_file()),
            pr_state: Arc::new(Mutex::new(PrCacheState::default())),
        }
    }

    pub fn with_git_binary(mut self, git_binary: PathBuf) -> Self {
        self.git_binary = git_binary;
        self
    }

    pub fn with_gh_binary(mut self, gh_binary: PathBuf) -> Self {
        self.gh_binary = gh_binary;
        self.gh_available = binary_available(&self.gh_binary);
        self
    }

    /// Override the disk cache location. Pass `None` to disable disk
    /// persistence entirely (used by tests that must not touch `$HOME`).
    pub fn with_cache_path(mut self, path: Option<PathBuf>) -> Self {
        self.cache_path = path;
        self
    }

    pub fn gh_available(&self) -> bool {
        self.gh_available
    }

    pub fn pr_enrichment_enabled(&self) -> bool {
        self.config.show_pull_requests && self.gh_available
    }

    pub fn watch(&self) -> DashboardWatch {
        let (rows_tx, rows_rx) = mpsc::channel(8);
        let (notice_tx, notice_rx) = mpsc::channel(8);
        let (refresh_tx, mut refresh_rx) = mpsc::channel(1);
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        let service = self.clone();
        if let Ok(mut state) = service.pr_state.lock() {
            state.notice_tx = Some(notice_tx.clone());
        }

        tokio::spawn(async move {
            let interval_ms = service.config.refresh_interval_ms;
            let mut interval = time::interval(Duration::from_millis(interval_ms));
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                // Emit git-only rows (with cached PRs applied) first so the
                // UI exits "Loading dashboard..." without waiting on the gh
                // GraphQL round-trip. Then refresh PRs and emit again.
                match service.collect_git_rows().await {
                    Ok(mut rows) => {
                        if rows_tx
                            .send(DashboardUpdate::GitOnly(rows.clone()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if service.pr_enrichment_enabled() {
                            service.refresh_pull_requests(&rows).await;
                            service.apply_cached_prs(&mut rows);
                            service.save_cache();
                            if rows_tx.send(DashboardUpdate::WithPRs(rows)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        let _ = notice_tx
                            .send(DashboardNotice::error(format!(
                                "Dashboard refresh failed: {err}"
                            )))
                            .await;
                    }
                }

                tokio::select! {
                    _ = &mut cancel_rx => break,
                    _ = interval.tick() => {}
                    maybe_refresh = refresh_rx.recv() => {
                        if maybe_refresh.is_none() {
                            break;
                        }
                    }
                }
            }
        });

        DashboardWatch {
            rx: rows_rx,
            notice_rx,
            cancel: Some(cancel_tx),
            refresh_tx,
        }
    }

    pub async fn snapshot(&self) -> Result<Vec<DashboardRow>> {
        let mut rows = self.collect_git_rows().await?;
        if self.pr_enrichment_enabled() {
            self.refresh_pull_requests(&rows).await;
            self.apply_cached_prs(&mut rows);
            self.save_cache();
        }
        Ok(rows)
    }

    /// Gather worktree + git-derived state (status, upstream diff, last commit)
    /// for every worktree in parallel, then layer cached PR data on top. No
    /// network calls — safe to emit immediately so the UI can render before
    /// the slower `gh` refresh completes.
    async fn collect_git_rows(&self) -> Result<Vec<DashboardRow>> {
        let worktrees = self.list_worktrees_basic().await?;
        let mut tasks = JoinSet::new();

        for worktree in worktrees {
            let service = self.clone();
            tasks.spawn(async move { service.enrich_worktree_git(worktree).await });
        }

        let mut rows = Vec::new();
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(row) => rows.push(row),
                Err(err) => {
                    return Err(WisetreeError::other(format!(
                        "Dashboard refresh task failed: {err}"
                    )));
                }
            }
        }

        self.ensure_cache_loaded();

        let live_branches: HashSet<String> =
            rows.iter().map(|row| row.worktree.branch.clone()).collect();
        self.prune_cache(&live_branches);

        self.apply_cached_prs(&mut rows);

        rows.sort_by_key(|row| (!row.worktree.is_main, row.worktree.path.clone()));
        Ok(rows)
    }

    async fn list_worktrees_basic(&self) -> Result<Vec<GitWorktree>> {
        let result =
            execute_git_command(&["worktree", "list", "--porcelain"], Some(&self.git_root)).await;
        if !result.success {
            return Err(handle_git_error(&result.stderr, "list worktrees"));
        }

        let mut worktrees = Vec::new();
        let mut current = GitWorktree::default();
        let mut have_current = false;

        for line in result.stdout.split('\n') {
            if let Some(path) = line.strip_prefix("worktree ") {
                if have_current {
                    worktrees.push(std::mem::take(&mut current));
                }
                current = GitWorktree {
                    path: path.to_string(),
                    ..GitWorktree::default()
                };
                have_current = true;
            } else if let Some(commit) = line.strip_prefix("HEAD ") {
                current.commit = commit.to_string();
            } else if let Some(branch) = line.strip_prefix("branch ") {
                current.branch = branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_string();
            } else if line == "bare" {
                current.is_main = true;
            } else if line.is_empty() && have_current {
                worktrees.push(std::mem::take(&mut current));
                have_current = false;
            }
        }

        if have_current {
            worktrees.push(current);
        }

        if let Some(first) = worktrees.first_mut() {
            first.is_main = true;
        }

        for worktree in &mut worktrees {
            if worktree.branch.is_empty() {
                worktree.branch = "detached".to_string();
            }
        }

        Ok(worktrees)
    }

    async fn enrich_worktree_git(&self, mut worktree: GitWorktree) -> DashboardRow {
        let mut errors = Vec::new();

        match self.fetch_status(Path::new(&worktree.path)).await {
            Ok((is_clean, branch_status)) => {
                worktree.is_clean = is_clean;
                worktree.branch_status = branch_status;
            }
            Err(err) => errors.push(format!("status: {err}")),
        }

        let last_commit = match self.fetch_last_commit(Path::new(&worktree.path)).await {
            Ok(commit) => commit,
            Err(err) => {
                errors.push(format!("last commit: {err}"));
                None
            }
        };

        DashboardRow {
            worktree,
            last_commit,
            pull_request: None,
            error: (!errors.is_empty()).then(|| errors.join("; ")),
        }
    }

    async fn fetch_status(
        &self,
        cwd: &Path,
    ) -> std::result::Result<(bool, Option<BranchStatus>), String> {
        let output = time::timeout(
            COMMAND_TIMEOUT,
            run_command(&self.git_binary, &["status", "--porcelain=v2"], Some(cwd)),
        )
        .await
        .map_err(|_| "timed out after 1s".to_string())??;

        let mut dirty = false;
        for line in output.lines() {
            if !line.trim().is_empty() && !line.starts_with('#') {
                dirty = true;
                break;
            }
        }

        let branch_status = self.fetch_upstream_diff(cwd).await;
        Ok((!dirty, branch_status))
    }

    /// Compute the line-level diff (insertions/deletions) of HEAD relative to
    /// the first reachable ref in `upstream/main`, `upstream/master`,
    /// `origin/main`, `origin/master`. Insertions are stored in `ahead` and
    /// deletions in `behind` so the renderer can show `+<ins> -<del>`.
    /// Returns `None` when none of those remote refs are reachable.
    async fn fetch_upstream_diff(&self, cwd: &Path) -> Option<BranchStatus> {
        for upstream in [
            "upstream/main",
            "upstream/master",
            "origin/main",
            "origin/master",
        ] {
            let result = time::timeout(
                COMMAND_TIMEOUT,
                run_command(
                    &self.git_binary,
                    &["diff", "--shortstat", upstream],
                    Some(cwd),
                ),
            )
            .await
            .ok()?;
            let Ok(output) = result else { continue };
            let (insertions, deletions) = parse_shortstat(&output);
            return Some(BranchStatus {
                ahead: insertions,
                behind: deletions,
                upstream_branch: Some(upstream.to_string()),
            });
        }
        None
    }

    async fn fetch_last_commit(
        &self,
        cwd: &Path,
    ) -> std::result::Result<Option<CommitSummary>, String> {
        let output = time::timeout(
            COMMAND_TIMEOUT,
            run_command(
                &self.git_binary,
                &["log", "-1", "--format=%h%x1f%s%x1f%cr%x1f%an"],
                Some(cwd),
            ),
        )
        .await
        .map_err(|_| "timed out after 1s".to_string())??;

        if output.trim().is_empty() {
            return Ok(None);
        }

        let parts: Vec<&str> = output.split('\u{1f}').collect();
        if parts.len() != 4 {
            return Err("unexpected git log output".to_string());
        }

        Ok(Some(CommitSummary {
            sha: parts[0].to_string(),
            summary: parts[1].to_string(),
            relative_time: parts[2].to_string(),
            author: parts[3].to_string(),
        }))
    }

    /// Decide which branches need a PR refresh, then (if any) issue a single
    /// batched GraphQL request and update the cache.
    async fn refresh_pull_requests(&self, rows: &[DashboardRow]) {
        if !self.pr_enrichment_enabled() {
            return;
        }
        if self.is_rate_limited() {
            return;
        }

        let now = now_ms();
        let to_fetch: Vec<(String, String)> = {
            let state = self.pr_state.lock().expect("pr_state poisoned");
            rows.iter()
                .filter_map(|row| {
                    let branch = row.worktree.branch.clone();
                    let sha = row.worktree.commit.clone();
                    if branch.is_empty() || branch == "detached" || sha.is_empty() {
                        return None;
                    }
                    let needs = match state.entries.get(&branch) {
                        Some(entry) => {
                            entry.sha != sha
                                || now.saturating_sub(entry.fetched_at_ms) > PR_CACHE_TTL_MS
                        }
                        None => true,
                    };
                    needs.then_some((branch, sha))
                })
                .collect()
        };

        if to_fetch.is_empty() {
            return;
        }

        let Some((owner, repo)) = self.resolve_repo_slug().await else {
            return;
        };

        let branches: Vec<&str> = to_fetch.iter().map(|(b, _)| b.as_str()).collect();
        match self.fetch_prs_batched(&owner, &repo, &branches).await {
            Ok(results) => {
                let mut state = self.pr_state.lock().expect("pr_state poisoned");
                let now = now_ms();
                for (branch, sha) in &to_fetch {
                    let pr = results.get(branch).cloned().flatten();
                    state.entries.insert(
                        branch.clone(),
                        PrCacheEntry {
                            sha: sha.clone(),
                            fetched_at_ms: now,
                            pull_request: pr,
                        },
                    );
                }
                // Successful round-trip — clear any prior rate-limit state.
                state.rate_limited_until = None;
                state.rate_limit_notice_sent = false;
            }
            Err(err) => {
                if is_rate_limit_error(&err) {
                    self.mark_rate_limited();
                } else {
                    self.mark_pr_refresh_failed(&err);
                }
                // Failures fall back to cached or empty PR data. Surface a
                // single dashboard-level notice instead of per-row errors,
                // because this GraphQL request covers every branch at once.
            }
        }
    }

    fn apply_cached_prs(&self, rows: &mut [DashboardRow]) {
        let state = self.pr_state.lock().expect("pr_state poisoned");
        for row in rows {
            if let Some(entry) = state.entries.get(&row.worktree.branch) {
                row.pull_request = entry.pull_request.clone();
            }
        }
    }

    fn is_rate_limited(&self) -> bool {
        let mut state = self.pr_state.lock().expect("pr_state poisoned");
        match state.rate_limited_until {
            Some(deadline) if Instant::now() < deadline => true,
            Some(_) => {
                state.rate_limited_until = None;
                state.rate_limit_notice_sent = false;
                false
            }
            None => false,
        }
    }

    fn mark_rate_limited(&self) {
        let notice = {
            let mut state = self.pr_state.lock().expect("pr_state poisoned");
            state.rate_limited_until = Some(Instant::now() + RATE_LIMIT_BACKOFF);
            if state.rate_limit_notice_sent {
                None
            } else {
                state.rate_limit_notice_sent = true;
                Some((
                    state.notice_tx.clone(),
                    DashboardNotice::warning(
                        "GitHub API rate-limited — pausing PR refresh for 5 min; showing cached data.",
                    ),
                ))
            }
        };
        if let Some((Some(tx), notice)) = notice {
            let _ = tx.try_send(notice);
        }
    }

    fn mark_pr_refresh_failed(&self, err: &str) {
        let notice = {
            let state = self.pr_state.lock().expect("pr_state poisoned");
            state.notice_tx.clone().map(|tx| {
                let summary = summarize_notice_text(err);
                let message = format!("GitHub PR refresh failed: {summary} — showing cached data.");
                (tx, DashboardNotice::error(message))
            })
        };
        if let Some((tx, notice)) = notice {
            let _ = tx.try_send(notice);
        }
    }

    async fn resolve_repo_slug(&self) -> Option<(String, String)> {
        if let Ok(state) = self.pr_state.lock() {
            if let Some(slug) = &state.repo_slug {
                return Some(slug.clone());
            }
        }

        // Prefer `upstream` over `origin` so fork-based workflows resolve to
        // the repository that actually hosts the PRs. Matches the precedence
        // used by `fetch_upstream_diff`.
        let mut slug = None;
        for remote in ["upstream", "origin"] {
            let Ok(result) = time::timeout(
                COMMAND_TIMEOUT,
                run_command(
                    &self.git_binary,
                    &["remote", "get-url", remote],
                    Some(&self.git_root),
                ),
            )
            .await
            else {
                continue;
            };
            let Ok(url) = result else { continue };
            if let Some(parsed) = parse_github_slug(&url) {
                slug = Some(parsed);
                break;
            }
        }

        let slug = slug?;
        if let Ok(mut state) = self.pr_state.lock() {
            state.repo_slug = Some(slug.clone());
        }
        Some(slug)
    }

    async fn fetch_prs_batched(
        &self,
        owner: &str,
        repo: &str,
        branches: &[&str],
    ) -> std::result::Result<HashMap<String, Option<PullRequest>>, String> {
        let query = build_graphql_query(owner, repo, branches);
        let arg = format!("query={query}");
        let output = time::timeout(
            GH_GRAPHQL_TIMEOUT,
            run_command(
                &self.gh_binary,
                &["api", "graphql", "-f", &arg],
                Some(&self.git_root),
            ),
        )
        .await
        .map_err(|_| "timed out after 8s".to_string())??;

        parse_graphql_response(&output, branches)
    }

    fn ensure_cache_loaded(&self) {
        let needs_load = {
            let state = self.pr_state.lock().expect("pr_state poisoned");
            !state.loaded_from_disk
        };
        if !needs_load {
            return;
        }

        let key = self.git_root.to_string_lossy().to_string();
        let mut loaded = HashMap::new();
        if let Some(path) = &self.cache_path {
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(parsed) = serde_json::from_str::<DiskCache>(&content) {
                    if let Some(entries) = parsed.get(&key) {
                        loaded = entries.clone();
                    }
                }
            }
        }

        let mut state = self.pr_state.lock().expect("pr_state poisoned");
        state.entries = loaded;
        state.loaded_from_disk = true;
    }

    fn prune_cache(&self, live_branches: &HashSet<String>) {
        let mut state = self.pr_state.lock().expect("pr_state poisoned");
        state
            .entries
            .retain(|branch, _| live_branches.contains(branch));
    }

    fn save_cache(&self) {
        let Some(path) = self.cache_path.clone() else {
            return;
        };
        let key = self.git_root.to_string_lossy().to_string();
        let entries = {
            let state = self.pr_state.lock().expect("pr_state poisoned");
            state.entries.clone()
        };

        // Merge with what's already on disk so other repos' entries survive.
        let mut disk: DiskCache = std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default();
        if entries.is_empty() {
            disk.remove(&key);
        } else {
            disk.insert(key, entries);
        }

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&disk) {
            let _ = std::fs::write(&path, json);
        }
    }
}

fn binary_available(binary: &Path) -> bool {
    std::process::Command::new(binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn is_rate_limit_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("rate limit") || lower.contains("rate-limit")
}

fn summarize_notice_text(message: &str) -> String {
    let compact = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        "unknown error".to_string()
    } else {
        compact
    }
}

/// Extract `(owner, repo)` from a GitHub remote URL. Handles the common SSH,
/// HTTPS, and `git@` SCP-style forms.
fn parse_github_slug(remote: &str) -> Option<(String, String)> {
    let trimmed = remote.trim();
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let trimmed = trimmed.trim_end_matches('/');
    let (_, after_host) = trimmed.rsplit_once("github.com")?;
    let path = after_host.trim_start_matches([':', '/']);
    let mut parts = path.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

fn build_graphql_query(owner: &str, repo: &str, branches: &[&str]) -> String {
    let mut q = String::new();
    q.push_str("query { repository(owner: \"");
    q.push_str(&escape_graphql_string(owner));
    q.push_str("\", name: \"");
    q.push_str(&escape_graphql_string(repo));
    q.push_str("\") { ");
    for (i, branch) in branches.iter().enumerate() {
        q.push_str(&format!(
            "b{i}: pullRequests(headRefName: \"{}\", first: 1, orderBy: {{field: CREATED_AT, direction: DESC}}) {{ nodes {{ number url title state isDraft commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state contexts(first: 100) {{ nodes {{ __typename ... on CheckRun {{ status conclusion }} ... on StatusContext {{ state }} }} }} }} }} }} }} }} }} ",
            escape_graphql_string(branch)
        ));
    }
    q.push_str("} }");
    q
}

fn escape_graphql_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn parse_graphql_response(
    body: &str,
    branches: &[&str],
) -> std::result::Result<HashMap<String, Option<PullRequest>>, String> {
    let envelope: GhEnvelope =
        serde_json::from_str(body).map_err(|err| format!("invalid gh response: {err}"))?;

    if let Some(errors) = envelope.errors {
        let joined = errors
            .into_iter()
            .map(|e| e.message)
            .collect::<Vec<_>>()
            .join("; ");
        if !joined.is_empty() {
            return Err(joined);
        }
    }

    let data = envelope.data.ok_or_else(|| "missing data".to_string())?;
    let repo = data
        .get("repository")
        .ok_or_else(|| "missing repository in response".to_string())?;

    let mut out: HashMap<String, Option<PullRequest>> = HashMap::new();
    for (i, branch) in branches.iter().enumerate() {
        let key = format!("b{i}");
        let pr = repo
            .get(&key)
            .and_then(|v| serde_json::from_value::<GhConnection>(v.clone()).ok())
            .and_then(|conn| conn.nodes.into_iter().next())
            .map(|node| {
                let state = if node.is_draft {
                    PrState::Draft
                } else {
                    match node.state.as_str() {
                        "OPEN" => PrState::Open,
                        "MERGED" => PrState::Merged,
                        "CLOSED" => PrState::Closed,
                        _ => PrState::Closed,
                    }
                };
                let checks_status = node
                    .commits
                    .nodes
                    .into_iter()
                    .next()
                    .and_then(|c| c.commit)
                    .and_then(|c| c.status_check_rollup)
                    .and_then(|r| aggregate_checks(&r.contexts.nodes));
                PullRequest {
                    number: node.number,
                    state,
                    url: node.url,
                    title: node.title,
                    checks_status,
                }
            });
        out.insert((*branch).to_string(), pr);
    }
    Ok(out)
}

#[derive(Deserialize)]
struct GhContextNode {
    #[serde(rename = "__typename", default)]
    typename: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
}
#[derive(Deserialize, Default)]
struct GhContexts {
    #[serde(default)]
    nodes: Vec<GhContextNode>,
}
#[derive(Deserialize)]
struct GhStatusCheckRollup {
    #[serde(default)]
    contexts: GhContexts,
}
#[derive(Deserialize)]
struct GhCommit {
    #[serde(rename = "statusCheckRollup", default)]
    status_check_rollup: Option<GhStatusCheckRollup>,
}
#[derive(Deserialize)]
struct GhCommitWrapper {
    #[serde(default)]
    commit: Option<GhCommit>,
}
#[derive(Deserialize, Default)]
struct GhCommits {
    #[serde(default)]
    nodes: Vec<GhCommitWrapper>,
}
#[derive(Deserialize)]
struct GhNode {
    number: u64,
    state: String,
    url: String,
    title: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    #[serde(default)]
    commits: GhCommits,
}
#[derive(Deserialize)]
struct GhConnection {
    nodes: Vec<GhNode>,
}
#[derive(Deserialize)]
struct GhError {
    message: String,
}
#[derive(Deserialize)]
struct GhEnvelope {
    #[serde(default)]
    data: Option<serde_json::Value>,
    #[serde(default)]
    errors: Option<Vec<GhError>>,
}

/// Aggregate raw check-run + status-context nodes into a single
/// [`CheckStatus`]. Returns `None` when no contexts are present so the
/// dashboard can render a plain "Opened" label without a circle.
///
/// Precedence (worst-case wins):
/// `Failed` > `Errored` > `Running` > `Pending` > `Passed`.
fn aggregate_checks(contexts: &[GhContextNode]) -> Option<CheckStatus> {
    if contexts.is_empty() {
        return None;
    }
    let mut acc: Option<CheckStatus> = None;
    for ctx in contexts {
        let candidate = match ctx.typename.as_str() {
            "CheckRun" => {
                let status = ctx.status.as_deref().unwrap_or("").to_ascii_uppercase();
                let conclusion = ctx.conclusion.as_deref().unwrap_or("").to_ascii_uppercase();
                match status.as_str() {
                    "QUEUED" | "WAITING" | "PENDING" | "REQUESTED" => Some(CheckStatus::Pending),
                    "IN_PROGRESS" => Some(CheckStatus::Running),
                    "COMPLETED" => match conclusion.as_str() {
                        "SUCCESS" | "NEUTRAL" | "SKIPPED" | "STALE" => Some(CheckStatus::Passed),
                        "FAILURE" => Some(CheckStatus::Failed),
                        "ACTION_REQUIRED" | "CANCELLED" | "TIMED_OUT" | "STARTUP_FAILURE" => {
                            Some(CheckStatus::Errored)
                        }
                        "" => None,
                        _ => Some(CheckStatus::Errored),
                    },
                    _ => None,
                }
            }
            "StatusContext" => {
                let state = ctx.state.as_deref().unwrap_or("").to_ascii_uppercase();
                match state.as_str() {
                    "EXPECTED" => Some(CheckStatus::Pending),
                    "PENDING" => Some(CheckStatus::Running),
                    "SUCCESS" => Some(CheckStatus::Passed),
                    "FAILURE" => Some(CheckStatus::Failed),
                    "ERROR" => Some(CheckStatus::Errored),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(candidate) = candidate {
            let beats = match acc {
                None => true,
                Some(existing) => check_priority(candidate) > check_priority(existing),
            };
            if beats {
                acc = Some(candidate);
            }
        }
    }
    acc
}

fn check_priority(status: CheckStatus) -> u8 {
    match status {
        CheckStatus::Passed => 0,
        CheckStatus::Pending => 1,
        CheckStatus::Running => 2,
        CheckStatus::Errored => 3,
        CheckStatus::Failed => 4,
    }
}

/// Parse `git diff --shortstat` output and return `(insertions, deletions)`.
/// Sample inputs:
///   ` 28 files changed, 2815 insertions(+), 42 deletions(-)`
///   ` 1 file changed, 5 insertions(+)`
///   `` (no diff → both zero)
fn parse_shortstat(output: &str) -> (u64, u64) {
    let mut insertions = 0u64;
    let mut deletions = 0u64;
    let tokens: Vec<&str> = output.split_whitespace().collect();
    for window in tokens.windows(2) {
        let Ok(num) = window[0].parse::<u64>() else {
            continue;
        };
        let label = window[1].trim_end_matches(',');
        if label.starts_with("insertion") {
            insertions = num;
        } else if label.starts_with("deletion") {
            deletions = num;
        }
    }
    (insertions, deletions)
}

async fn run_command(
    binary: &Path,
    args: &[&str],
    cwd: Option<&Path>,
) -> std::result::Result<String, String> {
    let mut cmd = Command::new(binary);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }

    let output = cmd.output().await.map_err(|err| err.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

pub fn default_dashboard_warning(config: &DashboardConfig, gh_available: bool) -> Option<String> {
    if config.show_pull_requests && !gh_available {
        Some("gh CLI not found - PR column hidden.".to_string())
    } else {
        None
    }
}

pub fn resolve_dashboard_columns(
    columns: &[String],
    pr_enrichment_enabled: bool,
) -> (Vec<String>, Vec<String>) {
    let (normalized, warnings) = normalize_dashboard_columns(columns);
    let mut resolved = Vec::new();

    for column in normalized {
        if column == "pull_request" && !pr_enrichment_enabled {
            continue;
        }
        resolved.push(column);
    }

    if resolved.is_empty() {
        resolved = vec![
            "branch".to_string(),
            "status".to_string(),
            "ahead_behind".to_string(),
            "last_commit".to_string(),
        ];
    }

    (resolved, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_ssh_scp_form() {
        assert_eq!(
            parse_github_slug("git@github.com:victorcorcos/wisetree.git"),
            Some(("victorcorcos".into(), "wisetree".into()))
        );
    }

    #[test]
    fn parses_github_https_form() {
        assert_eq!(
            parse_github_slug("https://github.com/victorcorcos/wisetree.git"),
            Some(("victorcorcos".into(), "wisetree".into()))
        );
    }

    #[test]
    fn parses_github_https_no_suffix() {
        assert_eq!(
            parse_github_slug("https://github.com/victorcorcos/wisetree"),
            Some(("victorcorcos".into(), "wisetree".into()))
        );
    }

    #[test]
    fn parses_github_ssh_url_form() {
        assert_eq!(
            parse_github_slug("ssh://git@github.com/foo/bar.git"),
            Some(("foo".into(), "bar".into()))
        );
    }

    #[test]
    fn rejects_non_github_remote() {
        assert_eq!(parse_github_slug("git@gitlab.com:foo/bar.git"), None);
    }

    #[test]
    fn detects_rate_limit_message() {
        assert!(is_rate_limit_error(
            "GraphQL: API rate limit exceeded for user ID 7637806."
        ));
        assert!(is_rate_limit_error("Secondary rate-limit triggered"));
        assert!(!is_rate_limit_error("network is unreachable"));
    }

    #[test]
    fn builds_graphql_query_with_aliases_per_branch() {
        let q = build_graphql_query("owner", "repo", &["feat/a", "fix-b"]);
        assert!(q.contains("b0: pullRequests(headRefName: \"feat/a\""));
        assert!(q.contains("b1: pullRequests(headRefName: \"fix-b\""));
        assert!(q.contains("repository(owner: \"owner\", name: \"repo\")"));
    }

    #[test]
    fn graphql_query_includes_status_check_rollup_for_each_branch() {
        let q = build_graphql_query("owner", "repo", &["feat"]);
        assert!(
            q.contains("statusCheckRollup"),
            "query must request statusCheckRollup so the dashboard can colour the Opened circle: {q}"
        );
        assert!(q.contains("commits(last: 1)"));
        assert!(q.contains("__typename"));
        assert!(q.contains("CheckRun"));
        assert!(q.contains("StatusContext"));
    }

    #[test]
    fn parses_graphql_response_into_branch_map() {
        let body = r#"{
          "data": {
            "repository": {
              "b0": {"nodes": [{"number": 7, "state": "OPEN", "url": "u", "title": "t", "isDraft": false}]},
              "b1": {"nodes": []}
            }
          }
        }"#;
        let out = parse_graphql_response(body, &["feat", "fix"]).unwrap();
        assert_eq!(out.get("feat").unwrap().as_ref().unwrap().number, 7);
        assert!(out.get("fix").unwrap().is_none());
    }

    #[test]
    fn parses_graphql_errors_envelope() {
        let body = r#"{"errors":[{"message":"API rate limit exceeded"}]}"#;
        let err = parse_graphql_response(body, &["feat"]).unwrap_err();
        assert!(is_rate_limit_error(&err));
    }

    fn check_run(status: &str, conclusion: &str) -> GhContextNode {
        GhContextNode {
            typename: "CheckRun".to_string(),
            status: Some(status.to_string()),
            conclusion: Some(conclusion.to_string()),
            state: None,
        }
    }

    fn status_context(state: &str) -> GhContextNode {
        GhContextNode {
            typename: "StatusContext".to_string(),
            status: None,
            conclusion: None,
            state: Some(state.to_string()),
        }
    }

    #[test]
    fn empty_contexts_yield_no_check_status() {
        assert_eq!(aggregate_checks(&[]), None);
    }

    #[test]
    fn running_beats_passed_in_aggregation() {
        let nodes = vec![
            check_run("IN_PROGRESS", ""),
            check_run("COMPLETED", "SUCCESS"),
        ];
        assert_eq!(aggregate_checks(&nodes), Some(CheckStatus::Running));
    }

    #[test]
    fn failed_beats_running_in_aggregation() {
        let nodes = vec![
            check_run("IN_PROGRESS", ""),
            check_run("COMPLETED", "FAILURE"),
        ];
        assert_eq!(aggregate_checks(&nodes), Some(CheckStatus::Failed));
    }

    #[test]
    fn errored_beats_running_but_loses_to_failed() {
        let mixed = vec![
            check_run("IN_PROGRESS", ""),
            check_run("COMPLETED", "TIMED_OUT"),
        ];
        assert_eq!(aggregate_checks(&mixed), Some(CheckStatus::Errored));

        let with_failure = vec![
            check_run("COMPLETED", "TIMED_OUT"),
            check_run("COMPLETED", "FAILURE"),
        ];
        assert_eq!(aggregate_checks(&with_failure), Some(CheckStatus::Failed));
    }

    #[test]
    fn errored_conclusions_cover_drone_failure_modes() {
        for conclusion in [
            "ACTION_REQUIRED",
            "CANCELLED",
            "TIMED_OUT",
            "STARTUP_FAILURE",
        ] {
            assert_eq!(
                aggregate_checks(&[check_run("COMPLETED", conclusion)]),
                Some(CheckStatus::Errored),
                "{conclusion} should be Errored"
            );
        }
    }

    #[test]
    fn status_context_states_map_to_check_statuses() {
        // Drone CI / legacy status API contributes StatusContext nodes.
        assert_eq!(
            aggregate_checks(&[status_context("EXPECTED")]),
            Some(CheckStatus::Pending)
        );
        assert_eq!(
            aggregate_checks(&[status_context("PENDING")]),
            Some(CheckStatus::Running)
        );
        assert_eq!(
            aggregate_checks(&[status_context("SUCCESS")]),
            Some(CheckStatus::Passed)
        );
        assert_eq!(
            aggregate_checks(&[status_context("FAILURE")]),
            Some(CheckStatus::Failed)
        );
        assert_eq!(
            aggregate_checks(&[status_context("ERROR")]),
            Some(CheckStatus::Errored)
        );
    }

    #[test]
    fn parses_graphql_response_with_check_status() {
        let body = r#"{
          "data": {
            "repository": {
              "b0": {"nodes": [{
                "number": 9,
                "state": "OPEN",
                "url": "u",
                "title": "t",
                "isDraft": false,
                "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": [
                  {"__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": null}
                ]}}}}]}
              }]}
            }
          }
        }"#;
        let out = parse_graphql_response(body, &["feat"]).unwrap();
        let pr = out.get("feat").unwrap().as_ref().unwrap();
        assert_eq!(pr.checks_status, Some(CheckStatus::Running));
    }

    #[test]
    fn parses_graphql_response_without_checks_keeps_status_none() {
        let body = r#"{
          "data": {
            "repository": {
              "b0": {"nodes": [{
                "number": 1,
                "state": "OPEN",
                "url": "u",
                "title": "t",
                "isDraft": false
              }]}
            }
          }
        }"#;
        let out = parse_graphql_response(body, &["feat"]).unwrap();
        let pr = out.get("feat").unwrap().as_ref().unwrap();
        assert_eq!(pr.checks_status, None);
    }

    #[test]
    fn parses_graphql_response_with_unknown_typename_does_not_panic() {
        let body = r#"{
          "data": {
            "repository": {
              "b0": {"nodes": [{
                "number": 1,
                "state": "OPEN",
                "url": "u",
                "title": "t",
                "isDraft": false,
                "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": [
                  {"__typename": "FuturisticThing"}
                ]}}}}]}
              }]}
            }
          }
        }"#;
        let out = parse_graphql_response(body, &["feat"]).unwrap();
        // Unknown contexts are ignored; with no recognized contexts the
        // PR has no aggregated check status (so the dashboard renders a
        // plain "Opened" label).
        assert_eq!(
            out.get("feat").unwrap().as_ref().unwrap().checks_status,
            None
        );
    }

    #[test]
    fn resolves_dashboard_columns_hides_pr_when_enrichment_disabled() {
        let (columns, warnings) = resolve_dashboard_columns(
            &["branch".into(), "pull_request".into(), "status".into()],
            false,
        );
        assert_eq!(columns, vec!["branch", "status"]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn resolves_dashboard_columns_keeps_pr_when_enrichment_enabled() {
        let (columns, warnings) = resolve_dashboard_columns(
            &["branch".into(), "pull_request".into(), "status".into()],
            true,
        );
        assert_eq!(columns, vec!["branch", "pull_request", "status"]);
        assert!(warnings.is_empty());
    }
}

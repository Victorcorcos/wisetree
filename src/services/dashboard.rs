//! Live dashboard polling service.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::{self, MissedTickBehavior};

use crate::config::schema::{normalize_dashboard_columns, DashboardConfig};
use crate::constants::dashboard_pr_cache_file;
use crate::errors::{handle_git_error, Result, WisetreeError};

use crate::git::exec::execute_git_command;
use crate::git::lock::{git_lock_path, retry_on_git_lock};

use crate::git::types::{BranchStatus, GitWorktree};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
/// `gh api graphql` may include the network round-trip — give it more headroom
/// than local git calls.
const GH_GRAPHQL_TIMEOUT: Duration = Duration::from_secs(8);
/// `gh pr merge` may wait on branch protections, required reviews, or remote
/// merge processing, so it deserves a longer leash than the read paths.
const PR_MERGE_TIMEOUT: Duration = Duration::from_secs(60);

const PUSH_TIMEOUT: Duration = Duration::from_secs(60);
/// Bound on the background single-branch base fetch that keeps the base's
/// remote-tracking ref fresh for the behind-count. A scoped fetch normally
/// completes in ~1s; the cap stops a slow network from stalling the on-cycle
/// tick.
const BASE_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Base refs used to calculate the Dashboard ahead/behind counts.
pub const BASE_REF_PRIORITY: [&str; 6] = [
    "upstream/main",
    "upstream/master",
    "upstream/develop",
    "origin/main",
    "origin/master",
    "origin/develop",
];
/// Remotes searched, in order, for a base branch or the repository hosting
/// the PRs: `upstream` first so fork-based workflows reach the canonical repo.
const CONVENTIONAL_REMOTES: [&str; 2] = ["upstream", "origin"];
/// Remote a PR update is pushed to when the branch has no push destination
/// of its own yet (see [`resolve_push_remote`]).
const FALLBACK_PUSH_REMOTE: &str = "origin";
/// How often the service refetches PR data when branches are otherwise idle.
/// Catches remote-only changes (merge, close, title edit) without hammering
/// the API. The Status column countdown is driven by the same timer.
pub const PR_REFRESH_PERIOD_MS: u64 = 30 * 1000;

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

/// Aggregated review status for the PR, derived from GitHub's
/// `reviewDecision` plus pending reviewer requests. Drives the secondary
/// emoji rendered next to the check status in the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewStatus {
    Pending,
    Approved,
    Rejected,
}

/// Merge readiness of a PR branch, derived from GitHub's `mergeStateStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeStatus {
    Draft,
    Dirty,
    Blocked,
    Unknown,
    Behind,
    HasHooks,
    Unstable,
    Clean,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub state: PrState,
    pub url: String,
    pub title: String,
    #[serde(
        rename = "baseRefName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub base_ref_name: Option<String>,
    #[serde(
        rename = "baseRepository",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub base_repository: Option<String>,
    #[serde(
        rename = "headRefOid",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub head_ref_oid: Option<String>,
    /// `owner/repo` the PR's head branch lives in — where pushes that update
    /// it must go. `None` when GitHub cannot name it (a deleted fork).
    #[serde(
        rename = "headRepository",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub head_repository: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(
        rename = "checksStatus",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub checks_status: Option<CheckStatus>,
    #[serde(
        rename = "reviewStatus",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub review_status: Option<ReviewStatus>,
    #[serde(
        rename = "mergeStatus",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub merge_status: Option<MergeStatus>,
    /// Reviewers grouped by how they currently appear in GitHub's Reviewers
    /// panel. Each list holds the bare `@login` (no leading `@`). Filled in
    /// from the GraphQL response so the dashboard can attribute the review
    /// status emoji to specific people.
    #[serde(
        rename = "reviewerSummary",
        default,
        skip_serializing_if = "ReviewerSummary::is_empty"
    )]
    pub reviewers: ReviewerSummary,
}

/// Lists of reviewers split by current status. Kept sorted so renders are
/// deterministic and dedup'd so a reviewer who re-requested doesn't appear
/// twice. Pending = was asked but hasn't reviewed yet (or was re-requested).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerSummary {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approved: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes_requested: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commented: Vec<String>,
}

impl ReviewerSummary {
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
            && self.approved.is_empty()
            && self.changes_requested.is_empty()
            && self.commented.is_empty()
    }
}

/// Title + body for a single pull request, fetched on demand by the merge
/// confirmation screen. Kept separate from `PullRequest` (which lives in the
/// dashboard cache and is intentionally lean) so PR descriptions never bloat
/// the persistent cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestDetails {
    pub title: String,
    pub body: String,
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
    Success,
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
/// `WithPRs` carries `next_pr_fetch_at`: the instant when the service will
/// run the next on-cycle PR refresh. The UI countdown renders directly
/// from this, so the displayed timer and the actual refresh are always
/// in sync.
#[derive(Debug)]
pub enum DashboardUpdate {
    GitOnly(Vec<DashboardRow>),
    WithPRs {
        rows: Vec<DashboardRow>,
        next_pr_fetch_at: Option<Instant>,
    },
}

impl DashboardUpdate {
    pub fn rows(&self) -> &Vec<DashboardRow> {
        match self {
            Self::GitOnly(rows) => rows,
            Self::WithPRs { rows, .. } => rows,
        }
    }

    pub fn into_rows(self) -> Vec<DashboardRow> {
        match self {
            Self::GitOnly(rows) => rows,
            Self::WithPRs { rows, .. } => rows,
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
    /// Set whenever `entries` is mutated by application code (PR insert,
    /// prune-on-branch-disappear). `save_cache` checks this flag and skips
    /// the read-merge-write cycle when nothing has changed since the last
    /// persist — avoiding unnecessary disk I/O on every refresh tick.
    dirty: bool,
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

    fn require_gh(&self) -> Result<()> {
        if !self.gh_available {
            return Err(WisetreeError::other(
                "gh CLI not found — install `gh` to use pull request features.",
            ));
        }
        Ok(())
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
            let period = pr_refresh_period(&service.config);
            // Single source of truth for when the next on-cycle PR fetch
            // is due. The UI countdown reads this verbatim, and the loop
            // wakes precisely at this instant so the fetch fires the moment
            // the countdown hits 0.
            let mut next_pr_fetch_at: Option<Instant> = None;

            loop {
                let enrich = service.pr_enrichment_enabled();
                let on_cycle = enrich && next_pr_fetch_at.map_or(true, |due| Instant::now() >= due);
                let mut base_refs = Vec::new();
                // Emit git-only rows (with cached PRs applied) first so the
                // UI exits "Loading dashboard..." instantly, without waiting on
                // any network round-trip. PR enrichment and the base-ref fetch
                // below are refinements that fill in afterwards.
                match service.collect_git_rows().await {
                    Ok(mut rows) => {
                        base_refs = distinct_base_refs(&rows);
                        if rows_tx
                            .send(DashboardUpdate::GitOnly(rows.clone()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if enrich {
                            service.refresh_pull_requests(&rows, on_cycle).await;
                            if on_cycle {
                                next_pr_fetch_at = Some(Instant::now() + period);
                            }
                            service.apply_cached_prs(&mut rows);
                            service.save_cache();
                            if rows_tx
                                .send(DashboardUpdate::WithPRs {
                                    rows,
                                    next_pr_fetch_at,
                                })
                                .await
                                .is_err()
                            {
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

                // On the 30s PR beat, refresh the base branches' remote-tracking
                // refs so the behind-count reflects commits another developer
                // pushed to a base. Runs after the paint above so it never
                // blocks the first render; when it actually advances a ref,
                // loop again right away to re-render the refined behind-count.
                // Best-effort: a failed or no-op fetch just falls through to
                // the normal wait.
                if on_cycle && service.fetch_base_refs(&base_refs).await {
                    continue;
                }

                // Wake on whichever fires first: the git interval (snappy
                // local data), the PR deadline (the countdown hitting 0),
                // a manual refresh, or cancel. Aligning the wake-up to the
                // deadline is what keeps `Status (✔)` visible for ~1s.
                let pr_sleep = next_pr_fetch_at
                    .map(|due| due.saturating_duration_since(Instant::now()))
                    .unwrap_or(period);
                tokio::select! {
                    _ = &mut cancel_rx => break,
                    _ = interval.tick() => {}
                    _ = tokio::time::sleep(pr_sleep) => {}
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
            // Snapshot serves cached PR data when available; new branches and
            // SHA changes still trigger a fetch, but unchanged branches reuse
            // the cache so repeated `wisetree dashboard` calls don't hammer
            // the gh API.
            let _ = self.refresh_pull_requests(&rows, false).await;
            self.apply_cached_prs(&mut rows);
            self.save_cache();
        }
        Ok(rows)
    }

    /// Fetch the latest title + body for a single pull request via
    /// `gh pr view`. Bypasses the dashboard cache so the merge confirmation
    /// screen always shows the description GitHub currently has. `repo` is
    /// the PR's repository (see [`pr_repository`]).
    pub async fn fetch_pr_details(
        &self,
        number: u64,
        repo: Option<&str>,
    ) -> Result<PullRequestDetails> {
        self.fetch_pr_details_with_repo(number, repo).await
    }

    async fn fetch_pr_details_with_repo(
        &self,
        number: u64,
        repo_slug: Option<&str>,
    ) -> Result<PullRequestDetails> {
        self.require_gh()?;
        let number_arg = number.to_string();
        let mut args = vec![
            "pr".to_string(),
            "view".to_string(),
            number_arg,
            "--json".to_string(),
            "title,body".to_string(),
        ];
        if let Some(repo_slug) = repo_slug {
            args.push("--repo".to_string());
            args.push(repo_slug.to_string());
        }
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = with_timeout(
            "gh pr view",
            GH_GRAPHQL_TIMEOUT,
            run_command(&self.gh_binary, &args_ref, Some(&self.git_root)),
        )
        .await?
        .map_err(WisetreeError::other)?;

        parse_pr_view_json(&output)
    }

    /// Squash-merge a pull request, passing the supplied subject and body
    /// straight through to `gh pr merge` so the resulting commit message is
    /// byte-for-byte the PR's title + description, with a trailing
    /// ` (#N)` reference appended to the subject to match GitHub's
    /// default squash-merge convention (the `#N` is auto-linked to the
    /// PR by GitHub's web UI).
    pub async fn merge_pull_request(
        &self,
        number: u64,
        subject: &str,
        body: &str,
        repo: Option<&str>,
    ) -> Result<()> {
        self.merge_pull_request_with_options(number, subject, body, repo, None)
            .await
    }

    /// Number of commits on the worktree's local `HEAD` that have not yet
    /// been pushed to its tracking remote (`@{upstream}`). Powers the Merge
    /// confirm guard: a squash-merge only ever includes what GitHub already
    /// has, so unpushed local commits are silently dropped unless the user
    /// pushes them first. Returns 0 when the tracking ref is unconfigured or
    /// the count can't be parsed — a safe fallback that leaves the existing
    /// merge-straight-away flow untouched.
    pub async fn unpushed_commit_count(&self, worktree_path: &str) -> u64 {
        local_ahead_of_tracking(&self.git_binary, &PathBuf::from(worktree_path)).await
    }

    /// Push the worktree's `HEAD` to its push remote
    /// (`git push <remote> HEAD`, see [`resolve_push_remote`]). Used by the
    /// Merge flow to flush local commits into the PR before it is
    /// squash-merged, so nothing is lost.
    pub async fn push_head(&self, worktree_path: &str) -> Result<()> {
        let cwd = PathBuf::from(worktree_path);
        let remote = resolve_push_remote_with_binary(&self.git_binary, &cwd).await;
        with_timeout(
            "git push",
            PUSH_TIMEOUT,
            run_command(&self.git_binary, &["push", &remote, "HEAD"], Some(&cwd)),
        )
        .await?
        .map_err(WisetreeError::other)?;
        Ok(())
    }

    async fn merge_pull_request_with_options(
        &self,
        number: u64,
        subject: &str,
        body: &str,
        repo_slug: Option<&str>,
        match_head_commit: Option<&str>,
    ) -> Result<()> {
        self.require_gh()?;
        let number_arg = number.to_string();
        let subject_with_ref = subject_with_pr_reference(subject, number);
        let mut args = vec![
            "pr".to_string(),
            "merge".to_string(),
            number_arg,
            "--squash".to_string(),
            "--subject".to_string(),
            subject_with_ref,
            "--body".to_string(),
            body.to_string(),
        ];
        if let Some(repo_slug) = repo_slug {
            args.push("--repo".to_string());
            args.push(repo_slug.to_string());
        }
        if let Some(match_head_commit) = match_head_commit {
            args.push("--match-head-commit".to_string());
            args.push(match_head_commit.to_string());
        }
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        with_timeout(
            "gh pr merge",
            PR_MERGE_TIMEOUT,
            run_command(&self.gh_binary, &args_ref, Some(&self.git_root)),
        )
        .await?
        .map_err(WisetreeError::other)?;
        Ok(())
    }

    /// Close a pull request via `gh pr close <number>`, in its own
    /// repository `repo` (see [`pr_repository`]).
    pub async fn close_pull_request(&self, number: u64, repo: Option<&str>) -> Result<()> {
        self.require_gh()?;
        let number_arg = number.to_string();
        let mut args = vec!["pr", "close", &number_arg];
        if let Some(repo) = repo {
            args.extend(["--repo", repo]);
        }
        with_timeout(
            "gh pr close",
            PR_MERGE_TIMEOUT,
            run_command(&self.gh_binary, &args, Some(&self.git_root)),
        )
        .await?
        .map_err(WisetreeError::other)?;
        Ok(())
    }

    /// Gather worktree + git-derived state (status, upstream diff, last commit)
    /// for every worktree in parallel, then layer cached PR data on top. No
    /// network calls — safe to emit immediately so the UI can render before
    /// the slower `gh` refresh completes.
    async fn collect_git_rows(&self) -> Result<Vec<DashboardRow>> {
        let worktrees = self.list_worktrees_basic().await?;
        let mut tasks = JoinSet::new();
        self.ensure_cache_loaded();

        for worktree in worktrees {
            let service = self.clone();
            let pr_base = self.cached_pr_base(&worktree.branch);
            tasks.spawn(async move { service.enrich_worktree_git(worktree, pr_base).await });
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

    /// Base branch and repository of the PR cached for `branch`, so its row is
    /// measured against the PR's own base (see
    /// [`resolve_pr_base_ref_with_binary`]).
    fn cached_pr_base(&self, branch: &str) -> Option<PrBase> {
        let state = self.pr_state.lock().expect("pr_state poisoned");
        let pr = state.entries.get(branch)?.pull_request.as_ref()?;
        Some(PrBase {
            name: pr.base_ref_name.clone()?,
            repository: pr.base_repository.clone(),
        })
    }

    async fn enrich_worktree_git(
        &self,
        mut worktree: GitWorktree,
        pr_base: Option<PrBase>,
    ) -> DashboardRow {
        let mut errors = Vec::new();

        match self
            .fetch_status(Path::new(&worktree.path), pr_base.as_ref())
            .await
        {
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
        pr_base: Option<&PrBase>,
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

        let branch_status = self.fetch_upstream_diff(cwd, pr_base).await;
        Ok((!dirty, branch_status))
    }

    /// Compute the commit-level ahead/behind of HEAD relative to the base ref:
    /// the PR's own base when the row has one (`pr_base`, resolved through the
    /// PR's repository), else the ref the branch was cut from
    /// ([`resolve_base_ref_with_binary`]). `ahead`/`behind` come from
    /// `git rev-list --left-right --count` (matching `GitService::branch_status`),
    /// and `insertions`/`deletions` come from a follow-up
    /// `git diff --shortstat --merge-base <upstream>` so the "Diff" column
    /// renders the branch's own line-level changes (committed or not) since it
    /// was cut — never the inverse of commits the base gained in the meantime.
    /// Returns `None` when no base ref is reachable.
    async fn fetch_upstream_diff(
        &self,
        cwd: &Path,
        pr_base: Option<&PrBase>,
    ) -> Option<BranchStatus> {
        let upstream = resolve_pr_base_ref_with_binary(
            &self.git_binary,
            cwd,
            pr_base.map(|base| base.name.as_str()),
            pr_base.and_then(|base| base.repository.as_deref()),
        )
        .await?;
        let spec = format!("{upstream}...HEAD");
        let result = time::timeout(
            COMMAND_TIMEOUT,
            run_command(
                &self.git_binary,
                &["rev-list", "--left-right", "--count", &spec],
                Some(cwd),
            ),
        )
        .await
        .ok()?;
        let Ok(output) = result else { return None };
        let mut parts = output.split_whitespace();
        let behind = parts
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let ahead = parts
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        // `git diff --shortstat` is best-effort: a timeout or non-zero exit
        // leaves the line counts as `None` so the Diff column renders "-"
        // instead of a misleading "+0 -0".
        let (insertions, deletions) = match time::timeout(
            COMMAND_TIMEOUT,
            run_command(
                &self.git_binary,
                &["diff", "--shortstat", "--merge-base", &upstream],
                Some(cwd),
            ),
        )
        .await
        {
            Ok(Ok(stdout)) => {
                let (ins, del) = parse_shortstat(&stdout);
                (Some(ins), Some(del))
            }
            _ => (None, None),
        };

        Some(BranchStatus {
            ahead,
            behind,
            upstream_branch: Some(upstream),
            insertions,
            deletions,
        })
    }

    /// Best-effort refresh of every base branch's remote-tracking ref (one per
    /// distinct base the rows were measured against, e.g. `upstream/main` and
    /// `kaefer/main`) so the dashboard's behind-count reflects commits another
    /// developer pushed to any base. Without this the count is measured against
    /// a possibly-stale ref, so a behind-but-conflict-free PR never surfaces
    /// the "Update" command in repos that don't enforce up-to-date branches.
    ///
    /// Bases are fetched one after another: concurrent fetches in the same
    /// repository would race on `FETCH_HEAD`.
    ///
    /// Returns `true` only when a fetch actually advanced a base's
    /// remote-tracking ref, so the caller can re-render the refined
    /// behind-count exactly when there is new data — and skip the churn when
    /// every ref was already current.
    async fn fetch_base_refs(&self, base_refs: &[String]) -> bool {
        let mut advanced = false;
        for base_ref in base_refs {
            advanced |= self.fetch_base_ref(base_ref).await;
        }
        advanced
    }

    /// Fetches a single branch with an explicit destination refspec
    /// (`+<branch>:refs/remotes/<remote>/<branch>`) so only the one
    /// remote-tracking ref `fetch_upstream_diff` reads is updated — no
    /// `--all`, no extra branches, minimal transfer. A base that is not a
    /// remote-tracking ref is skipped. Failures (offline, auth, missing ref)
    /// are swallowed: a stale tracking ref just means the count lags until
    /// the next successful fetch, never a stalled tick.
    async fn fetch_base_ref(&self, base_ref: &str) -> bool {
        let Some((remote, branch)) = base_ref.split_once('/') else {
            return false;
        };
        let tracking_ref = format!("refs/remotes/{base_ref}");
        let before = self.rev_parse(&tracking_ref).await;
        if before.is_none() {
            return false;
        }
        let refspec = format!("+{branch}:{tracking_ref}");
        let _ = time::timeout(
            BASE_FETCH_TIMEOUT,
            run_command(
                &self.git_binary,
                &["fetch", remote, &refspec],
                Some(&self.git_root),
            ),
        )
        .await;
        let after = self.rev_parse(&tracking_ref).await;
        after.is_some() && after != before
    }

    /// Resolve a ref to its commit OID against `git_root`, or `None` when the
    /// ref is missing. Used to detect whether `fetch_base_ref` moved the base.
    async fn rev_parse(&self, reference: &str) -> Option<String> {
        run_command(
            &self.git_binary,
            &["rev-parse", "--verify", "--quiet", reference],
            Some(&self.git_root),
        )
        .await
        .ok()
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

    /// Decide which branches need a PR refresh, then (if any) issue one
    /// batched GraphQL request per repository and update the cache.
    ///
    /// Every branch is looked up in the default repository. A branch cut from
    /// a third remote (e.g. `kaefer/main`) is also looked up in that remote's
    /// repository, whose PR wins when it has one.
    ///
    /// `on_cycle` is decided by the watch loop based on `next_pr_fetch_at`:
    /// `true` once per refresh period, `false` between periods. Off-cycle
    /// runs still pick up brand-new branches and SHA changes so the UI
    /// keeps up with local commits without disturbing the cycle rhythm.
    async fn refresh_pull_requests(&self, rows: &[DashboardRow], on_cycle: bool) -> bool {
        if !self.pr_enrichment_enabled() || self.is_rate_limited() {
            return false;
        }

        let to_fetch: Vec<PrFetchTarget> = {
            let state = self.pr_state.lock().expect("pr_state poisoned");
            rows.iter()
                .filter_map(|row| {
                    let branch = row.worktree.branch.clone();
                    let sha = row.worktree.commit.clone();
                    if branch.is_empty() || branch == "detached" || sha.is_empty() {
                        return None;
                    }
                    let needs = match state.entries.get(&branch) {
                        Some(entry) => entry.sha != sha || on_cycle,
                        None => true,
                    };
                    needs.then(|| PrFetchTarget {
                        branch,
                        sha,
                        base_remote: row_base_remote(row),
                        worktree_path: PathBuf::from(&row.worktree.path),
                    })
                })
                .collect()
        };

        if to_fetch.is_empty() {
            return false;
        }

        let Some(default_repo) = self.resolve_repo_slug().await else {
            return false;
        };
        let source_repos = self.source_pr_repositories(&to_fetch, &default_repo).await;

        let mut queries = vec![(
            default_repo.clone(),
            to_fetch
                .iter()
                .map(|target| target.branch.as_str())
                .collect::<Vec<_>>(),
        )];
        for (target, source_repo) in to_fetch.iter().zip(&source_repos) {
            let Some(repo) = source_repo else { continue };
            match queries.iter_mut().find(|(queried, _)| queried == repo) {
                Some((_, branches)) => branches.push(&target.branch),
                None => queries.push((repo.clone(), vec![&target.branch])),
            }
        }

        let mut fetched = HashMap::new();
        let mut failure = None;
        for (repo, branches) in &queries {
            match self.fetch_prs_batched(&repo.0, &repo.1, branches).await {
                Ok(prs) => {
                    fetched.insert(repo, prs);
                }
                Err(err) => {
                    let rate_limited = is_rate_limit_error(&err);
                    failure = Some(err);
                    if rate_limited {
                        break;
                    }
                }
            }
        }

        let mut selected = Vec::new();
        for (target, source_repo) in to_fetch.iter().zip(&source_repos) {
            if let Some(pr) = self
                .select_branch_pr(&fetched, target, source_repo.as_ref(), &default_repo)
                .await
            {
                selected.push((target, pr));
            }
        }

        let refreshed = {
            let mut state = self.pr_state.lock().expect("pr_state poisoned");
            let refreshed = !selected.is_empty();
            for (target, pr) in selected {
                state.entries.insert(
                    target.branch.clone(),
                    PrCacheEntry {
                        sha: target.sha.clone(),
                        pull_request: pr,
                    },
                );
            }
            state.dirty |= refreshed;
            if failure.is_none() {
                // Successful round-trip — clear any prior rate-limit state.
                state.rate_limited_until = None;
                state.rate_limit_notice_sent = false;
            }
            refreshed
        };

        // Failures fall back to cached or empty PR data. Surface a single
        // dashboard-level notice instead of per-row errors, because each
        // GraphQL request covers many branches at once.
        match failure {
            Some(err) if is_rate_limit_error(&err) => self.mark_rate_limited(),
            Some(err) => self.mark_pr_refresh_failed(&err),
            None => {}
        }
        refreshed
    }

    /// For each branch to fetch, the GitHub repository of the third remote it
    /// was cut from (e.g. `kaefer/main` → the `kaefer` remote's repository),
    /// or `None` when the default repository already covers it.
    async fn source_pr_repositories(
        &self,
        to_fetch: &[PrFetchTarget],
        default_repo: &(String, String),
    ) -> Vec<Option<(String, String)>> {
        let mut repos = Vec::with_capacity(to_fetch.len());
        for target in to_fetch {
            let repo = match unconventional_remote(target.base_remote.as_deref()) {
                Some(remote) => self.remote_github_slug(remote).await,
                None => None,
            };
            repos.push(repo.filter(|repo| repo != default_repo));
        }
        repos
    }

    /// The target branch's PR once every repository it is looked up in has
    /// answered: the newest candidate that is this branch's own (see
    /// [`Self::is_own_pull_request`]), from the source repository first, then
    /// the default one. `None` while an answer is missing, so the cached
    /// entry is kept rather than cleared.
    async fn select_branch_pr(
        &self,
        fetched: &HashMap<&(String, String), HashMap<String, Vec<PullRequest>>>,
        target: &PrFetchTarget,
        source_repo: Option<&(String, String)>,
        default_repo: &(String, String),
    ) -> Option<Option<PullRequest>> {
        let mut own_repository = None;
        for repo in source_repo.into_iter().chain([default_repo]) {
            let candidates = fetched.get(repo)?.get(&target.branch);
            for candidate in candidates.into_iter().flatten() {
                if self
                    .is_own_pull_request(&target.worktree_path, candidate, &mut own_repository)
                    .await
                {
                    return Some(Some(candidate.clone()));
                }
            }
        }
        Some(None)
    }

    /// GitHub matches PRs to a branch by name alone, so another fork's PR
    /// from a same-named branch is a candidate too. A candidate is this
    /// branch's own when its head lives in the repository this branch is
    /// pushed to, or when HEAD already contains its head commit. One whose
    /// head repository GitHub cannot name (a deleted fork) is kept.
    /// `own_repository` caches that repository across a branch's candidates.
    async fn is_own_pull_request(
        &self,
        cwd: &Path,
        candidate: &PullRequest,
        own_repository: &mut Option<Option<String>>,
    ) -> bool {
        let Some(head_repository) = &candidate.head_repository else {
            return true;
        };
        if own_repository.is_none() {
            let slug = match own_push_remote(&self.git_binary, cwd).await {
                Some(remote) => self
                    .remote_github_slug(&remote)
                    .await
                    .map(|(owner, repo)| format!("{owner}/{repo}")),
                None => None,
            };
            *own_repository = Some(slug);
        }
        if own_repository
            .as_ref()
            .and_then(Option::as_deref)
            .is_some_and(|own| own.eq_ignore_ascii_case(head_repository))
        {
            return true;
        }
        let Some(head) = candidate.head_ref_oid.as_deref() else {
            return false;
        };
        time::timeout(
            COMMAND_TIMEOUT,
            run_command(
                &self.git_binary,
                &["merge-base", "--is-ancestor", head, "HEAD"],
                Some(cwd),
            ),
        )
        .await
        .is_ok_and(|result| result.is_ok())
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
        for remote in CONVENTIONAL_REMOTES {
            slug = self.remote_github_slug(remote).await;
            if slug.is_some() {
                break;
            }
        }

        let slug = slug?;
        if let Ok(mut state) = self.pr_state.lock() {
            state.repo_slug = Some(slug.clone());
        }
        Some(slug)
    }

    /// `(owner, repo)` of `remote` when its URL points at GitHub.
    async fn remote_github_slug(&self, remote: &str) -> Option<(String, String)> {
        let url = time::timeout(
            COMMAND_TIMEOUT,
            run_command(
                &self.git_binary,
                &["remote", "get-url", remote],
                Some(&self.git_root),
            ),
        )
        .await
        .ok()?
        .ok()?;
        parse_github_slug(&url)
    }

    async fn fetch_prs_batched(
        &self,
        owner: &str,
        repo: &str,
        branches: &[&str],
    ) -> std::result::Result<HashMap<String, Vec<PullRequest>>, String> {
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
        let before = state.entries.len();
        state
            .entries
            .retain(|branch, _| live_branches.contains(branch));
        if state.entries.len() != before {
            state.dirty = true;
        }
    }

    fn save_cache(&self) {
        let Some(path) = self.cache_path.clone() else {
            return;
        };
        let key = self.git_root.to_string_lossy().to_string();
        let entries = {
            let state = self.pr_state.lock().expect("pr_state poisoned");
            // Skip the read-merge-write cycle entirely when nothing has
            // changed since the last persist. Without this guard the cache
            // file is rewritten on every refresh tick (3–5s) even when no
            // PR entry actually moved.
            if !state.dirty {
                return;
            }
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
            if std::fs::write(&path, json).is_ok() {
                let mut state = self.pr_state.lock().expect("pr_state poisoned");
                state.dirty = false;
            }
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

fn is_rate_limit_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("rate limit") || lower.contains("rate-limit")
}

fn pr_refresh_period(_config: &DashboardConfig) -> Duration {
    Duration::from_millis(PR_REFRESH_PERIOD_MS)
}

/// The base ref the row's ahead/behind was measured against.
fn row_base_ref(row: &DashboardRow) -> Option<&str> {
    row.worktree
        .branch_status
        .as_ref()?
        .upstream_branch
        .as_deref()
}

/// The remote of the row's base ref.
fn row_base_remote(row: &DashboardRow) -> Option<String> {
    remote_name_from_ref(row_base_ref(row)?).map(str::to_string)
}

/// Each distinct base ref the rows were measured against, in sorted order.
fn distinct_base_refs(rows: &[DashboardRow]) -> Vec<String> {
    let base_refs: BTreeSet<&str> = rows.iter().filter_map(row_base_ref).collect();
    base_refs.into_iter().map(str::to_string).collect()
}

fn branch_name_from_ref(base_ref: &str) -> &str {
    base_ref
        .split_once('/')
        .map(|(_, branch)| branch)
        .unwrap_or(base_ref)
}

fn remote_name_from_ref(base_ref: &str) -> Option<&str> {
    base_ref.split_once('/').map(|(remote, _)| remote)
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
/// `owner/repo` of the repository a pull request belongs to, read from its
/// URL (`https://github.com/<owner>/<repo>/pull/<N>`). Every `gh` call that
/// selects a PR by number passes it as `--repo`: `gh` otherwise picks its
/// default repository (typically `upstream`), where a PR opened on another
/// remote's repository (e.g. `kaefer`) resolves to an unrelated PR with the
/// same number.
pub fn pr_repository(url: &str) -> Option<String> {
    parse_github_slug(url).map(|(owner, repo)| format!("{owner}/{repo}"))
}

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

/// PRs fetched per branch name, newest first: GitHub matches PRs to a branch
/// by name alone, so a newer PR from another fork's same-named branch must not
/// hide this branch's own.
const PR_CANDIDATES_PER_BRANCH: u8 = 5;

/// GitHub's base branch name of a PR and the repository it targets.
struct PrBase {
    name: String,
    repository: Option<String>,
}

/// A branch whose PR the dashboard refreshes.
struct PrFetchTarget {
    branch: String,
    sha: String,
    /// Remote of the base ref the row was measured against.
    base_remote: Option<String>,
    worktree_path: PathBuf,
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
            "b{i}: pullRequests(headRefName: \"{}\", states: [OPEN, CLOSED, MERGED], first: {PR_CANDIDATES_PER_BRANCH}, orderBy: {{field: CREATED_AT, direction: DESC}}) {{ nodes {{ number url title state isDraft baseRefName baseRepository {{ nameWithOwner }} headRefOid headRepository {{ nameWithOwner }} mergeStateStatus reviewDecision labels(first: 20) {{ nodes {{ name }} }} reviewRequests(first: 100) {{ totalCount nodes {{ requestedReviewer {{ __typename ... on User {{ login }} }} }} }} latestOpinionatedReviews(first: 100) {{ nodes {{ state author {{ login }} }} }} latestReviews(first: 100) {{ nodes {{ state author {{ login }} }} }} commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state contexts(first: 100) {{ nodes {{ __typename ... on CheckRun {{ status conclusion }} ... on StatusContext {{ state }} }} }} }} }} }} }} }} }} ",
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

/// Append ` (#N)` to a squash-merge subject, idempotently. GitHub's
/// default squash-merge commit title is `"<PR title> (#<PR number>)"`
/// — when we hand `gh pr merge` an explicit `--subject` it uses it
/// verbatim, so we reproduce the suffix here. The `#N` is plain text;
/// GitHub's web UI auto-links it back to the PR.
fn subject_with_pr_reference(subject: &str, number: u64) -> String {
    let trimmed = subject.trim_end();
    let suffix = format!("(#{number})");
    if trimmed.ends_with(&suffix) {
        return trimmed.to_string();
    }
    format!("{trimmed} {suffix}")
}

/// Parse the JSON `gh pr view <N> --json title,body` returns. Missing
/// fields default to empty strings — that's the right behavior for both
/// the title (would surprise but won't crash) and the body (open PRs are
/// allowed to have an empty description).
fn parse_pr_view_json(body: &str) -> Result<PullRequestDetails> {
    #[derive(Deserialize)]
    struct PrViewJson {
        #[serde(default)]
        title: String,
        #[serde(default)]
        body: String,
    }

    let parsed: PrViewJson = serde_json::from_str(body)
        .map_err(|err| WisetreeError::other(format!("invalid gh pr view output: {err}")))?;
    Ok(PullRequestDetails {
        title: parsed.title,
        body: parsed.body,
    })
}

fn parse_graphql_response(
    body: &str,
    branches: &[&str],
) -> std::result::Result<HashMap<String, Vec<PullRequest>>, String> {
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

    let mut out: HashMap<String, Vec<PullRequest>> = HashMap::new();
    for (i, branch) in branches.iter().enumerate() {
        let key = format!("b{i}");
        let candidates = repo
            .get(&key)
            .and_then(|v| serde_json::from_value::<GhConnection>(v.clone()).ok())
            .map(|conn| conn.nodes)
            .unwrap_or_default()
            .into_iter()
            .map(|node| {
                let head_repository = node
                    .head_repository
                    .as_ref()
                    .and_then(|repo| repo.name_with_owner.clone());
                // GitHub keeps `isDraft = true` on a PR that was closed while
                // still a draft, so the terminal states must win over the draft
                // flag — otherwise a closed draft keeps reading as "Drafted".
                // Priority: Merged > Closed > Drafted > Opened.
                let state = match node.state.as_str() {
                    "MERGED" => PrState::Merged,
                    "CLOSED" => PrState::Closed,
                    "OPEN" if node.is_draft => PrState::Draft,
                    "OPEN" => PrState::Open,
                    _ => PrState::Closed,
                };
                let checks_status = node
                    .commits
                    .nodes
                    .into_iter()
                    .next()
                    .and_then(|c| c.commit)
                    .and_then(|c| c.status_check_rollup)
                    .and_then(|r| aggregate_checks(&r.contexts.nodes));
                let requested_user_logins: HashSet<String> = node
                    .review_requests
                    .nodes
                    .iter()
                    .filter_map(|r| {
                        r.requested_reviewer.as_ref().and_then(|rev| {
                            if rev.typename == "User" {
                                rev.login.clone()
                            } else {
                                None
                            }
                        })
                    })
                    .collect();
                let changes_requested_logins: HashSet<String> = node
                    .latest_opinionated_reviews
                    .nodes
                    .iter()
                    .filter(|r| r.state.as_deref() == Some("CHANGES_REQUESTED"))
                    .filter_map(|r| r.author.as_ref().and_then(|a| a.login.clone()))
                    .collect();
                let review_status = derive_review_status(
                    node.review_decision.as_deref(),
                    node.review_requests.total_count,
                    &changes_requested_logins,
                    &requested_user_logins,
                );
                let reviewers =
                    build_reviewer_summary(&requested_user_logins, &node.latest_reviews.nodes);
                let merge_status = match node.merge_state_status.as_deref() {
                    Some("DRAFT") => Some(MergeStatus::Draft),
                    Some("DIRTY") => Some(MergeStatus::Dirty),
                    Some("BLOCKED") => Some(MergeStatus::Blocked),
                    Some("UNKNOWN") => Some(MergeStatus::Unknown),
                    Some("BEHIND") => Some(MergeStatus::Behind),
                    Some("HAS_HOOKS") => Some(MergeStatus::HasHooks),
                    Some("UNSTABLE") => Some(MergeStatus::Unstable),
                    Some("CLEAN") => Some(MergeStatus::Clean),
                    _ => None,
                };
                let labels = node
                    .labels
                    .nodes
                    .into_iter()
                    .filter(|l| !l.name.is_empty())
                    .map(|l| l.name)
                    .collect();
                PullRequest {
                    number: node.number,
                    state,
                    url: node.url,
                    title: node.title,
                    base_ref_name: node.base_ref_name,
                    base_repository: node.base_repository.and_then(|repo| repo.name_with_owner),
                    head_ref_oid: node.head_ref_oid,
                    head_repository,
                    labels,
                    checks_status,
                    review_status,
                    merge_status,
                    reviewers,
                }
            })
            .collect();
        out.insert((*branch).to_string(), candidates);
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
#[derive(Deserialize, Default)]
struct GhReviewRequests {
    #[serde(rename = "totalCount", default)]
    total_count: u64,
    #[serde(default)]
    nodes: Vec<GhReviewRequestNode>,
}
#[derive(Deserialize, Default)]
struct GhReviewRequestNode {
    #[serde(rename = "requestedReviewer", default)]
    requested_reviewer: Option<GhRequestedReviewer>,
}
#[derive(Deserialize, Default)]
struct GhRequestedReviewer {
    #[serde(rename = "__typename", default)]
    typename: String,
    #[serde(default)]
    login: Option<String>,
}
#[derive(Deserialize, Default)]
struct GhOpinionatedReviews {
    #[serde(default)]
    nodes: Vec<GhOpinionatedReviewNode>,
}
#[derive(Deserialize, Default)]
struct GhOpinionatedReviewNode {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    author: Option<GhReviewAuthor>,
}
#[derive(Deserialize, Default)]
struct GhLatestReviews {
    #[serde(default)]
    nodes: Vec<GhLatestReviewNode>,
}
#[derive(Deserialize, Default)]
struct GhLatestReviewNode {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    author: Option<GhReviewAuthor>,
}
#[derive(Deserialize, Default)]
struct GhReviewAuthor {
    #[serde(default)]
    login: Option<String>,
}
#[derive(Deserialize, Default)]
struct GhLabelNode {
    #[serde(default)]
    name: String,
}
#[derive(Deserialize, Default)]
struct GhLabels {
    #[serde(default)]
    nodes: Vec<GhLabelNode>,
}
#[derive(Deserialize, Default)]
struct GhBaseRepository {
    #[serde(rename = "nameWithOwner", default)]
    name_with_owner: Option<String>,
}
#[derive(Deserialize)]
struct GhNode {
    number: u64,
    state: String,
    url: String,
    title: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    #[serde(rename = "baseRefName", default)]
    base_ref_name: Option<String>,
    #[serde(rename = "baseRepository", default)]
    base_repository: Option<GhBaseRepository>,
    #[serde(rename = "headRefOid", default)]
    head_ref_oid: Option<String>,
    #[serde(rename = "headRepository", default)]
    head_repository: Option<GhBaseRepository>,
    #[serde(rename = "mergeStateStatus", default)]
    merge_state_status: Option<String>,
    #[serde(rename = "reviewDecision", default)]
    review_decision: Option<String>,
    #[serde(default)]
    labels: GhLabels,
    #[serde(rename = "reviewRequests", default)]
    review_requests: GhReviewRequests,
    #[serde(rename = "latestOpinionatedReviews", default)]
    latest_opinionated_reviews: GhOpinionatedReviews,
    #[serde(rename = "latestReviews", default)]
    latest_reviews: GhLatestReviews,
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

/// Translate GitHub's `reviewDecision` (plus the still-pending reviewer
/// requests and the users who left CHANGES_REQUESTED reviews) into a
/// [`ReviewStatus`]. Returns `None` when no one has been asked to review yet
/// so the dashboard renders nothing.
///
/// When a reviewer leaves CHANGES_REQUESTED and the author later re-requests
/// their review, GitHub keeps `reviewDecision` as `CHANGES_REQUESTED` even
/// though the PR is back in "Awaiting requested review" — the decision only
/// flips after the reviewer leaves a fresh review. We detect that case by
/// checking whether every user who left CHANGES_REQUESTED is currently
/// listed in the outstanding `reviewRequests`, and surface Pending so the
/// dashboard matches what the GitHub UI shows in the Reviewers section.
/// Build the per-reviewer breakdown the dashboard footer renders. The
/// `requested_user_logins` set drives the Pending bucket: a user is pending
/// whenever GitHub still lists them under `reviewRequests`, even if they
/// previously left a review the author has since re-requested. The Pending
/// bucket therefore wins over Approved / Changes-requested / Commented so
/// the dashboard matches the Reviewers panel on github.com.
fn build_reviewer_summary(
    requested_user_logins: &HashSet<String>,
    latest_review_nodes: &[GhLatestReviewNode],
) -> ReviewerSummary {
    let pending: HashSet<String> = requested_user_logins.clone();
    let mut approved: HashSet<String> = HashSet::new();
    let mut changes_requested: HashSet<String> = HashSet::new();
    let mut commented: HashSet<String> = HashSet::new();

    for review in latest_review_nodes {
        let Some(login) = review.author.as_ref().and_then(|a| a.login.clone()) else {
            continue;
        };
        if pending.contains(&login) {
            continue;
        }
        match review.state.as_deref() {
            Some("APPROVED") => {
                approved.insert(login);
            }
            Some("CHANGES_REQUESTED") => {
                changes_requested.insert(login);
            }
            Some("COMMENTED") => {
                commented.insert(login);
            }
            _ => {}
        }
    }

    let sorted = |set: HashSet<String>| {
        let mut v: Vec<String> = set.into_iter().collect();
        v.sort();
        v
    };

    ReviewerSummary {
        pending: sorted(pending),
        approved: sorted(approved),
        changes_requested: sorted(changes_requested),
        commented: sorted(commented),
    }
}

fn derive_review_status(
    decision: Option<&str>,
    pending_requests: u64,
    changes_requested_logins: &HashSet<String>,
    requested_user_logins: &HashSet<String>,
) -> Option<ReviewStatus> {
    match decision {
        Some("APPROVED") => Some(ReviewStatus::Approved),
        Some("CHANGES_REQUESTED") => {
            if !changes_requested_logins.is_empty()
                && changes_requested_logins.is_subset(requested_user_logins)
            {
                Some(ReviewStatus::Pending)
            } else {
                Some(ReviewStatus::Rejected)
            }
        }
        Some("REVIEW_REQUIRED") => Some(ReviewStatus::Pending),
        _ if pending_requests > 0 => Some(ReviewStatus::Pending),
        _ => None,
    }
}

/// Pull `(insertions, deletions)` out of `git diff --shortstat` output.
/// Shortstat looks like ` 4 files changed, 12 insertions(+), 3 deletions(-)`.
/// Either count can be absent — a pure-additions diff prints only
/// `insertions(+)`, a deletions-only diff only `deletions(-)`, and an empty
/// diff prints nothing at all (which we report as `(0, 0)`).
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

async fn with_timeout<T>(
    name: &str,
    timeout: Duration,
    fut: impl std::future::Future<Output = T>,
) -> Result<T> {
    time::timeout(timeout, fut)
        .await
        .map_err(|_| WisetreeError::other(format!("{name} timed out after {}s", timeout.as_secs())))
}

/// Run `binary <args>` in `cwd`, capturing both streams. Used for both `git`
/// and `gh`; git-lock contention is recovered transparently
/// ([`retry_on_git_lock`]) so every mutating dashboard git op (merge, push,
/// fetch, add, commit, revert, …) survives a crashed git process or a
/// concurrent lock holder. `gh` failures never match the lock signature, so
/// they pass straight through on the first try.
async fn run_command(
    binary: &Path,
    args: &[&str],
    cwd: Option<&Path>,
) -> std::result::Result<String, String> {
    retry_on_git_lock(
        || run_command_once(binary, args, cwd),
        |result: &std::result::Result<String, String>| {
            result
                .as_ref()
                .err()
                .and_then(|stderr| git_lock_path(stderr))
        },
    )
    .await
}

/// One `binary <args>` spawn, with no lock recovery — the retryable unit
/// behind [`run_command`].
async fn run_command_once(
    binary: &Path,
    args: &[&str],
    cwd: Option<&Path>,
) -> std::result::Result<String, String> {
    let mut cmd = Command::new(binary);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    // Tie child lifetime to ours: dashboard PR refreshes fire `gh` / `git`
    // on a 30s loop. If wisetree gets torn down between iterations we don't
    // want the in-flight subprocess to outlive us as a zombie.
    cmd.kill_on_drop(true);
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

/// Resolve the base ref for a worktree — the branch it was cut from, which
/// is what every PR command diffs and merges against. Preference order:
///
/// 1. `pr_base_ref` — GitHub's own `baseRefName` for an existing PR, mapped
///    onto a reachable remote-tracking ref. Authoritative when a PR exists.
/// 2. The ref the branch was cut from (see [`branch_source_ref`]).
/// 3. The conventional `BASE_REF_PRIORITY` list, as a last resort.
///
/// This replaces the old "first reachable in `BASE_REF_PRIORITY`" behavior,
/// which always resolved to `upstream/master` regardless of the branch's
/// real base. Used by the dashboard's behind probe and every PR command so
/// the resolution can never drift between them.
pub async fn resolve_base_ref(cwd: &Path, pr_base_ref: Option<&str>) -> Option<String> {
    resolve_base_ref_with_binary(Path::new("git"), cwd, pr_base_ref).await
}

pub(crate) async fn resolve_base_ref_with_binary(
    git_binary: &Path,
    cwd: &Path,
    pr_base_ref: Option<&str>,
) -> Option<String> {
    let source = branch_source_ref(git_binary, cwd).await;

    // 1. Prefer GitHub's known base branch for the PR, mapped to whichever
    //    remote-tracking ref we actually have locally.
    if let Some(name) = pr_base_ref.map(str::trim).filter(|n| !n.is_empty()) {
        let source_remote = source.as_deref().and_then(remote_name_from_ref);
        for remote in base_remotes(source_remote) {
            let candidate = format!("{remote}/{name}");
            if ref_is_reachable(git_binary, cwd, &candidate).await {
                return Some(candidate);
            }
        }
    }

    // 2. The ref the branch was cut from.
    if source.is_some() {
        return source;
    }

    // 3. Fall back to the conventional priority list.
    for candidate in BASE_REF_PRIORITY {
        if ref_is_reachable(git_binary, cwd, candidate).await {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Remotes a base branch name is mapped onto: the conventional
/// `upstream` → `origin` order, led by the remote the branch was cut from
/// when that is a third one (e.g. a second canonical repository such as
/// `kaefer`), so its `main` never resolves to an unrelated `upstream/main`.
fn base_remotes(source_remote: Option<&str>) -> Vec<&str> {
    let mut remotes = Vec::with_capacity(CONVENTIONAL_REMOTES.len() + 1);
    remotes.extend(unconventional_remote(source_remote));
    remotes.extend(CONVENTIONAL_REMOTES);
    remotes
}

/// `remote` when it is a third remote, neither `upstream` nor `origin`.
fn unconventional_remote(remote: Option<&str>) -> Option<&str> {
    remote.filter(|remote| !CONVENTIONAL_REMOTES.contains(remote))
}

/// The remote-tracking ref the current branch was cut from: its tracked
/// upstream (`@{upstream}`), which `git worktree add` seeds from the source
/// branch (e.g. `upstream/release-0.41`). Once a push with `-u` replaced that
/// with the branch's own pushed copy — on `origin` or any other remote, and
/// never a base — the start point its reflog recorded at creation is used.
async fn branch_source_ref(git_binary: &Path, cwd: &Path) -> Option<String> {
    let branch = current_branch_name(git_binary, cwd).await?;
    if let Some(upstream) = tracked_upstream(git_binary, cwd).await {
        if !is_own_pushed_ref(&upstream, &branch)
            && ref_is_reachable(git_binary, cwd, &upstream).await
        {
            return Some(upstream);
        }
    }
    reflog_source_ref(git_binary, cwd, &branch).await
}

/// The remote every push that updates the worktree's PR goes to: the one
/// `git push` sends the current branch to (`@{push}`), e.g. a `kaefer-fork`
/// the branch was pushed to with `-u` — where its PR's head lives — or, when
/// git has none, the remote the branch was pushed to without `-u` (see
/// [`pushed_copy_remote`]). Falls back to `origin` when neither exists. A
/// destination that is a differently named branch (such as the base it was
/// cut from) never counts: HEAD must never be pushed into it.
pub async fn resolve_push_remote(cwd: &Path) -> String {
    resolve_push_remote_with_binary(Path::new("git"), cwd).await
}

pub(crate) async fn resolve_push_remote_with_binary(git_binary: &Path, cwd: &Path) -> String {
    own_push_remote(git_binary, cwd)
        .await
        .unwrap_or_else(|| FALLBACK_PUSH_REMOTE.to_string())
}

async fn own_push_remote(git_binary: &Path, cwd: &Path) -> Option<String> {
    let branch = current_branch_name(git_binary, cwd).await?;
    match git_push_destination(git_binary, cwd, &branch).await {
        Some(remote) => Some(remote),
        None => pushed_copy_remote(git_binary, cwd, &branch).await,
    }
}

/// The remote of `@{push}` when it is `branch`'s own counterpart.
async fn git_push_destination(git_binary: &Path, cwd: &Path, branch: &str) -> Option<String> {
    let push_ref = time::timeout(
        COMMAND_TIMEOUT,
        run_command(
            git_binary,
            &["rev-parse", "--abbrev-ref", "@{push}"],
            Some(cwd),
        ),
    )
    .await
    .ok()?
    .ok()?;
    let push_ref = push_ref.trim();
    if !is_own_pushed_ref(push_ref, branch) {
        return None;
    }
    remote_name_from_ref(push_ref).map(str::to_string)
}

/// The one remote holding a copy of `branch` that HEAD already contains —
/// what `git push <remote> HEAD` without `-u` leaves behind while the branch
/// keeps tracking the base it was cut from. A same-named branch HEAD does not
/// contain belongs to someone else, and two candidates are ambiguous: both
/// yield `None`.
async fn pushed_copy_remote(git_binary: &Path, cwd: &Path, branch: &str) -> Option<String> {
    let remotes = time::timeout(
        COMMAND_TIMEOUT,
        run_command(git_binary, &["remote"], Some(cwd)),
    )
    .await
    .ok()?
    .ok()?;
    let mut holders = Vec::new();
    for remote in remotes.lines().map(str::trim).filter(|r| !r.is_empty()) {
        let copy = format!("refs/remotes/{remote}/{branch}");
        let contained = time::timeout(
            COMMAND_TIMEOUT,
            run_command(
                git_binary,
                &["merge-base", "--is-ancestor", &copy, "HEAD"],
                Some(cwd),
            ),
        )
        .await
        .is_ok_and(|result| result.is_ok());
        if contained {
            holders.push(remote.to_string());
        }
    }
    match holders.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// `true` when `remote_ref` (`<remote>/<branch>`) is `branch`'s own
/// counterpart on some remote rather than a branch it was cut from.
fn is_own_pushed_ref(remote_ref: &str, branch: &str) -> bool {
    branch_name_from_ref(remote_ref) == branch
}

/// The remote-tracking ref `branch` was created from, read from the
/// `branch: Created from <start-point>` entry that `git worktree add -b`,
/// `git branch` and `git switch -c` write first. `None` when that entry has
/// expired, or the start point was not a remote-tracking ref (a local
/// branch, `HEAD`, a commit) or was the branch's own remote counterpart.
async fn reflog_source_ref(git_binary: &Path, cwd: &Path, branch: &str) -> Option<String> {
    let reflog = time::timeout(
        COMMAND_TIMEOUT,
        run_command(
            git_binary,
            &[
                "reflog",
                "show",
                "--format=%gs",
                &format!("refs/heads/{branch}"),
            ],
            Some(cwd),
        ),
    )
    .await
    .ok()?
    .ok()?;
    let start_point = reflog
        .lines()
        .last()?
        .strip_prefix("branch: Created from ")?
        .trim();
    let start_point = start_point
        .strip_prefix("refs/remotes/")
        .unwrap_or(start_point);
    // `--abbrev-ref` also resolves a symbolic `<remote>/HEAD` to its branch.
    let source = time::timeout(
        COMMAND_TIMEOUT,
        run_command(
            git_binary,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--abbrev-ref",
                &format!("refs/remotes/{start_point}"),
            ],
            Some(cwd),
        ),
    )
    .await
    .ok()?
    .ok()?;
    let source = source.trim();
    (!source.is_empty() && !is_own_pushed_ref(source, branch)).then(|| source.to_string())
}

/// Resolve the local base ref of an existing PR: GitHub's base branch name
/// through the remote that points at the PR's repository (`pr_repository`,
/// e.g. `kaefer-cit/dpms-api-norway` → `kaefer/main`), so it never lands on a
/// same-named branch of an unrelated remote even when the worktree's own
/// source is unknown (a `gh pr checkout`, an expired reflog). Falls back to
/// [`resolve_base_ref`].
pub(crate) async fn resolve_pr_base_ref_with_binary(
    git_binary: &Path,
    cwd: &Path,
    pr_base_ref: Option<&str>,
    pr_repository: Option<&str>,
) -> Option<String> {
    let name = pr_base_ref.map(str::trim).filter(|name| !name.is_empty());
    if let (Some(name), Some(repository)) = (name, pr_repository) {
        if let Some(base_ref) =
            resolve_github_repository_ref(git_binary, cwd, name, repository).await
        {
            return Some(base_ref);
        }
    }
    resolve_base_ref_with_binary(git_binary, cwd, pr_base_ref).await
}

/// Resolve a branch only through a remote that points at the repository which
/// owns an existing pull request. This avoids mapping `main` to an unrelated
/// canonical `upstream` when the PR and its stack live in the user's fork.
async fn resolve_github_repository_ref(
    git_binary: &Path,
    cwd: &Path,
    branch: &str,
    repository: &str,
) -> Option<String> {
    let remotes = time::timeout(
        COMMAND_TIMEOUT,
        run_command(git_binary, &["remote"], Some(cwd)),
    )
    .await
    .ok()?
    .ok()?;
    let mut candidates = remotes
        .lines()
        .map(str::trim)
        .filter(|remote| !remote.is_empty())
        .collect::<Vec<_>>();
    candidates.sort_by_key(|remote| match *remote {
        "origin" => 0,
        "upstream" => 1,
        _ => 2,
    });
    for remote in candidates {
        let url = match time::timeout(
            COMMAND_TIMEOUT,
            run_command(git_binary, &["remote", "get-url", remote], Some(cwd)),
        )
        .await
        {
            Ok(Ok(url)) => url,
            _ => continue,
        };
        let remote_repository =
            parse_github_slug(&url).map(|(owner, repo)| format!("{owner}/{repo}"));
        if remote_repository.as_deref() != Some(repository) {
            continue;
        }
        let candidate = format!("{remote}/{}", branch.trim());
        if ref_is_reachable(git_binary, cwd, &candidate).await {
            return Some(candidate);
        }
    }
    None
}

/// `true` when `git rev-parse --verify` resolves `refname` in `cwd`. A
/// timeout or non-zero exit reads as unreachable so one slow/missing ref
/// never aborts the whole resolution.
async fn ref_is_reachable(git_binary: &Path, cwd: &Path, refname: &str) -> bool {
    time::timeout(
        COMMAND_TIMEOUT,
        run_command(
            git_binary,
            &["rev-parse", "--verify", "--quiet", refname],
            Some(cwd),
        ),
    )
    .await
    .map(|result| result.is_ok())
    .unwrap_or(false)
}

/// The remote-tracking branch the worktree's current branch is configured to
/// track (`branch.<name>.remote`/`.merge`), e.g. `upstream/release-0.41`.
/// `git worktree add` seeds this from the source branch, so before the branch
/// is pushed it names exactly the ref the worktree was cut from. `None` when
/// the branch tracks nothing (detached HEAD, or created from a raw commit).
async fn tracked_upstream(git_binary: &Path, cwd: &Path) -> Option<String> {
    run_command(
        git_binary,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
        Some(cwd),
    )
    .await
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

/// The worktree's current branch name, or `None` on a detached HEAD.
async fn current_branch_name(git_binary: &Path, cwd: &Path) -> Option<String> {
    run_command(
        git_binary,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        Some(cwd),
    )
    .await
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty() && s != "HEAD")
}

/// Number of commits that local HEAD has that the tracking remote
/// (`@{upstream}`) does not. Returns 0 when the tracking ref is not
/// configured or the count cannot be parsed — a safe fallback that
/// avoids a spurious push when the tracking state is unknown.
async fn local_ahead_of_tracking(git_binary: &Path, cwd: &Path) -> u64 {
    run_command(
        git_binary,
        &["rev-list", "--count", "@{upstream}..HEAD"],
        Some(cwd),
    )
    .await
    .ok()
    .and_then(|out| out.trim().parse::<u64>().ok())
    .unwrap_or(0)
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

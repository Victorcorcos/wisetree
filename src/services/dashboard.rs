//! Live dashboard polling service.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
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
/// `gh pr merge` may wait on branch protections, required reviews, or remote
/// merge processing, so it deserves a longer leash than the read paths.
const PR_MERGE_TIMEOUT: Duration = Duration::from_secs(60);
/// Timeouts for the update_pull_request pipeline. Network round-trips and
/// AI conflict resolution are inherently slower than the read-only paths,
/// so they each get their own ceiling.
const UPDATE_FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const UPDATE_MERGE_TIMEOUT: Duration = Duration::from_secs(120);
const UPDATE_PUSH_TIMEOUT: Duration = Duration::from_secs(60);
const UPDATE_GEMINI_TIMEOUT: Duration = Duration::from_secs(600);
const AI_TOOL_TIMEOUT: Duration = Duration::from_secs(60);
const AI_TOOL_OUTPUT_LIMIT: usize = 32_000;
const AI_TOOL_LOOP_LIMIT: usize = 24;
const GEMINI_CLI_BINARY: &str = "gemini";
/// Priority list for the base ref the "Update Pull Request" flow merges
/// in. Kept in one place so the dashboard's behind probe and the update
/// pipeline never drift apart.
pub const BASE_REF_PRIORITY: [&str; 4] = [
    "upstream/main",
    "upstream/master",
    "origin/main",
    "origin/master",
];
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

/// Live progress signal emitted by `update_pull_request` while the
/// pipeline runs. The UI consumes these to drive granular toasts (one per
/// phase transition) and a streaming activity panel that mirrors what the
/// AI is doing in real time. Pass `None` for the progress sender when the
/// caller doesn't care (e.g. integration tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiActivitySeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiToolResultStatus {
    Success,
    Error,
}

/// Structured event emitted by the streamed Gemini subprocess adapter.
/// Keeping semantic fields intact lets the TUI apply syntax-aware coloring
/// instead of flattening everything into plain white text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiActivityEvent {
    SessionStart {
        model: String,
    },
    AssistantText {
        content: String,
    },
    /// Gemini CLI's interactive UI exposes thought summaries; its current
    /// `stream-json` formatter drops them, but we keep the variant so the
    /// TUI is ready if upstream starts emitting them.
    Thinking {
        content: String,
    },
    ToolCall {
        tool_name: String,
        summary: String,
    },
    ToolResult {
        tool_name: Option<String>,
        status: AiToolResultStatus,
        detail: String,
    },
    Notice {
        severity: AiActivitySeverity,
        message: String,
    },
    Summary {
        tool_calls: u64,
        duration_ms: u64,
        total_tokens: u64,
    },
    Raw {
        text: String,
    },
}

impl AiActivityEvent {
    pub fn plain_text(&self) -> String {
        match self {
            Self::SessionStart { model } => format!("[session started · model: {model}]"),
            Self::AssistantText { content } => format!("AI: {content}"),
            Self::Thinking { content } => format!("Thinking: {content}"),
            Self::ToolCall { tool_name, summary } => {
                if summary.is_empty() {
                    format!("> {tool_name}()")
                } else {
                    format!("> {tool_name}({summary})")
                }
            }
            Self::ToolResult {
                tool_name,
                status,
                detail,
            } => {
                let label = match status {
                    AiToolResultStatus::Success => "ok",
                    AiToolResultStatus::Error => "error",
                };
                match tool_name {
                    Some(tool_name) => format!("< {tool_name} {label}: {detail}"),
                    None => format!("< tool {label}: {detail}"),
                }
            }
            Self::Notice { severity, message } => match severity {
                AiActivitySeverity::Info => format!("info: {message}"),
                AiActivitySeverity::Warning => format!("warning: {message}"),
                AiActivitySeverity::Error => format!("error: {message}"),
            },
            Self::Summary {
                tool_calls,
                duration_ms,
                total_tokens,
            } => format!(
                "[done · {tool_calls} tool calls · {:.1}s · {total_tokens} tokens]",
                *duration_ms as f64 / 1000.0
            ),
            Self::Raw { text } => text.clone(),
        }
    }
}

impl From<String> for AiActivityEvent {
    fn from(text: String) -> Self {
        Self::Raw { text }
    }
}

impl From<&str> for AiActivityEvent {
    fn from(text: &str) -> Self {
        Self::Raw {
            text: text.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateProgress {
    Phase(UpdatePhase),
    /// One structured activity event from the AI subprocess. ANSI escape
    /// sequences are already stripped so the UI can render the event
    /// directly.
    AiOutput(AiActivityEvent),
}

/// Coarse-grained pipeline phases. One phase fires before the matching
/// git/AI command runs (so the toast/spinner reflects what is *about* to
/// happen, not what just finished). Terminal outcomes are reported through
/// `UpdatePullRequestOutcome` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdatePhase {
    Fetching,
    AlreadyUpToDate,
    Merging,
    NoConflicts,
    ConflictsDetected { count: usize, model: String },
    AiResolving { model: String },
    Committing,
    Pushing,
}

/// Outcome of the `update_pull_request` pipeline. Surfaced verbatim to the
/// UI which maps each variant to a palette-colored toast or to a follow-up
/// screen (in the case of `MergedAwaitingReview`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdatePullRequestOutcome {
    /// The recheck after `git fetch` showed the branch was already up to
    /// date with the resolved base ref. No merge or push was attempted.
    AlreadyUpToDate,
    /// `git merge` succeeded with no conflicts; the result has been pushed.
    MergedCleanly,
    /// Gemini resolved at least one conflict and the merge commit was
    /// created with the standard message. The push has NOT been run yet —
    /// the UI must present the diff for review and let the user choose to
    /// push or discard. `diff` is the full `git diff HEAD~1 HEAD` so the
    /// review screen can render added/removed lines, not just per-file
    /// counts.
    MergedAwaitingReview {
        commit_sha: String,
        stat: String,
        diff: String,
    },
    /// Reviewed AI merge was pushed successfully. Only returned by
    /// `push_after_review`.
    MergedWithAiResolution,
    /// User chose to discard the AI merge and the reset succeeded. Only
    /// returned by `discard_after_review`.
    DiscardedAfterReview,
    /// Merge produced conflicts but Gemini was unavailable (for example no API
    /// key was configured for the direct client, or a test stub script was
    /// missing). The half-applied merge was aborted; the worktree is back to a
    /// clean state. The list of conflicted files is included so the toast can
    /// show how many files need attention.
    GeminiMissing { conflicts: Vec<String> },
    /// `git fetch` failed (network, auth, …). stderr included.
    FetchFailed(String),
    /// `git merge` failed for a non-conflict reason (e.g. dirty tree), or
    /// gemini ran but conflicts remained, or `git add`/`git commit` failed
    /// during AI resolution. stderr/details included.
    MergeFailed(String),
    /// `git push origin HEAD` failed. stderr included.
    PushFailed(String),
    /// `git reset --hard HEAD~1` failed during discard. stderr included.
    DiscardFailed(String),
}

/// Outcome of the `update_branch` pipeline (fetch + merge on the mother
/// worktree). The success variants split out what `git merge` actually
/// did so the UI can show a toast that reflects reality (a no-op vs a
/// fast-forward vs a real merge commit) instead of a generic "updated".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateBranchOutcome {
    /// `git merge` printed "Already up to date." — the worktree was
    /// already at or ahead of `base_ref`, nothing changed on disk.
    AlreadyUpToDate { base_ref: String },
    /// `git merge` advanced HEAD as a fast-forward (no merge commit).
    /// `summary` is the first non-empty line of git's stdout, which
    /// typically reads `Updating <old>..<new>` — useful for the toast.
    FastForwarded { base_ref: String, summary: String },
    /// `git merge` created a merge commit (divergent histories). The
    /// commit message is git's default `Merge ...`. `summary` carries
    /// the first non-empty line of git's stdout for the toast.
    Merged { base_ref: String, summary: String },
    /// None of the refs in `BASE_REF_PRIORITY` resolved against the
    /// worktree, even after `git fetch`. Nothing to merge against.
    NoBaseRef,
    /// `git fetch --all --prune` failed (network, auth, ...). stderr included.
    FetchFailed(String),
    /// `git merge {base_ref}` failed for any reason (conflicts, dirty
    /// tree, refusal). stderr included.
    MergeFailed { base_ref: String, message: String },
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum AiConflictResolver {
    /// Production path: talk directly to Gemini's streaming API.
    Direct,
    /// Test seam: execute a deterministic local script in place of the API.
    Script(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AiConflictResolutionError {
    Unavailable,
    Failed(String),
}

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
    /// Conflict-resolution backend. Production uses a direct Gemini API
    /// stream; tests can swap in a deterministic local script.
    ai_resolver: AiConflictResolver,
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
            ai_resolver: AiConflictResolver::Direct,
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

    /// Override the AI conflict-resolution backend with a deterministic local
    /// script. Used by tests to avoid live Gemini API calls.
    pub fn with_gemini_binary(mut self, gemini_binary: PathBuf) -> Self {
        self.ai_resolver = AiConflictResolver::Script(gemini_binary);
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
            let period = Duration::from_millis(PR_REFRESH_PERIOD_MS);
            // Single source of truth for when the next on-cycle PR fetch
            // is due. The UI countdown reads this verbatim, and the loop
            // wakes precisely at this instant so the fetch fires the moment
            // the countdown hits 0.
            let mut next_pr_fetch_at: Option<Instant> = None;

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
                            let on_cycle =
                                next_pr_fetch_at.map_or(true, |due| Instant::now() >= due);
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
            self.refresh_pull_requests(&rows, false).await;
            self.apply_cached_prs(&mut rows);
            self.save_cache();
        }
        Ok(rows)
    }

    /// Fetch the latest title + body for a single pull request via
    /// `gh pr view`. Bypasses the dashboard cache so the merge confirmation
    /// screen always shows the description GitHub currently has.
    pub async fn fetch_pr_details(&self, number: u64) -> Result<PullRequestDetails> {
        if !self.gh_available {
            return Err(WisetreeError::other(
                "gh CLI not found — install `gh` to fetch pull request details.",
            ));
        }
        let number_arg = number.to_string();
        let output = time::timeout(
            GH_GRAPHQL_TIMEOUT,
            run_command(
                &self.gh_binary,
                &["pr", "view", &number_arg, "--json", "title,body"],
                Some(&self.git_root),
            ),
        )
        .await
        .map_err(|_| WisetreeError::other("gh pr view timed out after 8s"))?
        .map_err(WisetreeError::other)?;

        parse_pr_view_json(&output)
    }

    /// Squash-merge a pull request, passing the supplied subject and body
    /// straight through to `gh pr merge` so the resulting commit message is
    /// byte-for-byte the PR's title + description, with a trailing
    /// ` (#N)` reference appended to the subject to match GitHub's
    /// default squash-merge convention (the `#N` is auto-linked to the
    /// PR by GitHub's web UI).
    pub async fn merge_pull_request(&self, number: u64, subject: &str, body: &str) -> Result<()> {
        if !self.gh_available {
            return Err(WisetreeError::other(
                "gh CLI not found — install `gh` to merge pull requests.",
            ));
        }
        let number_arg = number.to_string();
        let subject_with_ref = subject_with_pr_reference(subject, number);
        time::timeout(
            PR_MERGE_TIMEOUT,
            run_command(
                &self.gh_binary,
                &[
                    "pr",
                    "merge",
                    &number_arg,
                    "--squash",
                    "--subject",
                    &subject_with_ref,
                    "--body",
                    body,
                ],
                Some(&self.git_root),
            ),
        )
        .await
        .map_err(|_| WisetreeError::other("gh pr merge timed out after 60s"))?
        .map_err(WisetreeError::other)?;
        Ok(())
    }

    /// Drive the "Update Pull Request" pipeline against `worktree_path`.
    ///
    /// 1. `git fetch --all --prune`
    /// 2. Recheck behind count against `base_ref`; if zero → `AlreadyUpToDate`.
    /// 3. `git merge <base_ref>`.
    ///    - Exit 0 → push and return `MergedCleanly`.
    ///    - Non-zero → look for conflicts. If Gemini is unavailable,
    ///      abort the merge and return `GeminiMissing { conflicts }`.
    ///      Otherwise hand the worktree to the direct Gemini API client,
    ///      `git add -A`, `git commit -m "Merging and solving conflicts"`,
    ///      then capture commit SHA + stat and return
    ///      `MergedAwaitingReview { commit_sha, stat }` — the push is
    ///      deferred until the UI confirms via `push_after_review` /
    ///      `discard_after_review`.
    /// 4. Clean merges proceed straight to `git push origin HEAD`.
    pub async fn update_pull_request(
        &self,
        worktree_path: &str,
        base_ref: &str,
    ) -> Result<UpdatePullRequestOutcome> {
        self.update_pull_request_with_progress(worktree_path, base_ref, None)
            .await
    }

    /// Same pipeline as `update_pull_request`, but emits `UpdateProgress`
    /// events through `progress` as phases transition and (for AI runs)
    /// streams the subprocess's stdout/stderr line by line. Callers that
    /// don't need live feedback pass `None` and behave identically to
    /// the legacy entry point.
    pub async fn update_pull_request_with_progress(
        &self,
        worktree_path: &str,
        base_ref: &str,
        progress: Option<mpsc::UnboundedSender<UpdateProgress>>,
    ) -> Result<UpdatePullRequestOutcome> {
        let cwd = PathBuf::from(worktree_path);
        let send_phase = |phase: UpdatePhase| {
            if let Some(tx) = progress.as_ref() {
                let _ = tx.send(UpdateProgress::Phase(phase));
            }
        };

        // 1. fetch
        send_phase(UpdatePhase::Fetching);
        let fetch = time::timeout(
            UPDATE_FETCH_TIMEOUT,
            run_command(&self.git_binary, &["fetch", "--all", "--prune"], Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("git fetch timed out after 60s"))?;
        if let Err(err) = fetch {
            return Ok(UpdatePullRequestOutcome::FetchFailed(err));
        }

        // 2. recheck behind
        let behind = behind_against_base(&self.git_binary, &cwd, base_ref).await;
        if matches!(behind, Some(0)) {
            send_phase(UpdatePhase::AlreadyUpToDate);
            return Ok(UpdatePullRequestOutcome::AlreadyUpToDate);
        }

        // 3. merge
        send_phase(UpdatePhase::Merging);
        let merge = time::timeout(
            UPDATE_MERGE_TIMEOUT,
            run_command(&self.git_binary, &["merge", base_ref], Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("git merge timed out after 120s"))?;

        let ai_conflicts: Option<Vec<String>> = match merge {
            Ok(_) => None,
            Err(stderr) => {
                let conflicts = conflicted_files(&self.git_binary, &cwd).await;
                if conflicts.is_empty() {
                    // Merge failed for some non-conflict reason (e.g. dirty
                    // tree, refusing to merge). Bubble the stderr.
                    return Ok(UpdatePullRequestOutcome::MergeFailed(stderr));
                }
                Some(conflicts)
            }
        };

        if let Some(conflicts) = ai_conflicts {
            let model = crate::constants::UPDATE_GEMINI_MODEL.to_string();
            send_phase(UpdatePhase::ConflictsDetected {
                count: conflicts.len(),
                model: model.clone(),
            });
            send_phase(UpdatePhase::AiResolving {
                model: model.clone(),
            });
            let gemini_result = time::timeout(
                UPDATE_GEMINI_TIMEOUT,
                self.resolve_conflicts_with_ai(
                    &cwd,
                    &model,
                    base_ref,
                    &conflicts,
                    progress.clone(),
                ),
            )
            .await
            .map_err(|_| WisetreeError::other("gemini timed out after 10m"))?;
            if let Err(err) = gemini_result {
                let _ = run_command(&self.git_binary, &["merge", "--abort"], Some(&cwd)).await;
                return Ok(match err {
                    AiConflictResolutionError::Unavailable => {
                        UpdatePullRequestOutcome::GeminiMissing { conflicts }
                    }
                    AiConflictResolutionError::Failed(err) => {
                        UpdatePullRequestOutcome::MergeFailed(format!("gemini failed: {err}"))
                    }
                });
            }

            // Verify nothing remains in conflict state.
            let remaining = conflicted_files(&self.git_binary, &cwd).await;
            if !remaining.is_empty() {
                let _ = run_command(&self.git_binary, &["merge", "--abort"], Some(&cwd)).await;
                return Ok(UpdatePullRequestOutcome::MergeFailed(format!(
                    "conflicts unresolved after gemini: {}",
                    remaining.join(", ")
                )));
            }

            // Guard against catastrophic truncation (e.g. Gemini writing the
            // single word "resolved" to a file instead of merging it).
            if let Some(bad_file) =
                catastrophically_truncated(&self.git_binary, &cwd, &conflicts).await
            {
                let _ = run_command(&self.git_binary, &["merge", "--abort"], Some(&cwd)).await;
                return Ok(UpdatePullRequestOutcome::MergeFailed(format!(
                    "gemini replaced '{bad_file}' with near-empty content; \
                     refusing to commit a destructive resolution"
                )));
            }

            // Stage + commit.
            send_phase(UpdatePhase::Committing);
            if let Err(err) = run_command(&self.git_binary, &["add", "-A"], Some(&cwd)).await {
                return Ok(UpdatePullRequestOutcome::MergeFailed(format!(
                    "git add failed: {err}"
                )));
            }
            if let Err(err) = run_command(
                &self.git_binary,
                &[
                    "commit",
                    "-m",
                    crate::constants::UPDATE_MERGE_COMMIT_MESSAGE,
                ],
                Some(&cwd),
            )
            .await
            {
                return Ok(UpdatePullRequestOutcome::MergeFailed(format!(
                    "git commit failed: {err}"
                )));
            }

            // AI resolved — stop here and let the UI present the merge
            // commit for review before push. Capture SHA + stat + the full
            // diff against the parent commit (`HEAD~1`) so the review
            // screen can render added/removed lines, not just file stats.
            let commit_sha = run_command(
                &self.git_binary,
                &["rev-parse", "--short", "HEAD"],
                Some(&cwd),
            )
            .await
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "HEAD".to_string());
            let stat = run_command(
                &self.git_binary,
                &["show", "--stat", "--format=", "HEAD"],
                Some(&cwd),
            )
            .await
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
            let diff = run_command(&self.git_binary, &["diff", "HEAD~1", "HEAD"], Some(&cwd))
                .await
                .unwrap_or_default();
            return Ok(UpdatePullRequestOutcome::MergedAwaitingReview {
                commit_sha,
                stat,
                diff,
            });
        }

        // 4. push (clean merge path only — AI merges return above for review)
        send_phase(UpdatePhase::NoConflicts);
        send_phase(UpdatePhase::Pushing);
        let push = time::timeout(
            UPDATE_PUSH_TIMEOUT,
            run_command(&self.git_binary, &["push", "origin", "HEAD"], Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("git push timed out after 60s"))?;
        if let Err(err) = push {
            return Ok(UpdatePullRequestOutcome::PushFailed(err));
        }

        Ok(UpdatePullRequestOutcome::MergedCleanly)
    }

    /// Fetch the remote and merge the worktree at `worktree_path` with
    /// the first reachable ref in `BASE_REF_PRIORITY` (upstream/main →
    /// upstream/master → origin/main → origin/master). Powers the
    /// dashboard's "Update Branch" action on the mother worktree.
    pub async fn update_branch(&self, worktree_path: &str) -> Result<UpdateBranchOutcome> {
        let cwd = PathBuf::from(worktree_path);

        let fetch = time::timeout(
            UPDATE_FETCH_TIMEOUT,
            run_command(&self.git_binary, &["fetch", "--all", "--prune"], Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("git fetch timed out after 60s"))?;
        if let Err(err) = fetch {
            return Ok(UpdateBranchOutcome::FetchFailed(err));
        }

        let Some(base_ref) = resolve_base_ref_with_binary(&self.git_binary, &cwd).await else {
            return Ok(UpdateBranchOutcome::NoBaseRef);
        };

        let merge = time::timeout(
            UPDATE_MERGE_TIMEOUT,
            run_command(&self.git_binary, &["merge", &base_ref], Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("git merge timed out after 120s"))?;

        match merge {
            Ok(stdout) => Ok(classify_merge_output(base_ref, &stdout)),
            Err(stderr) => Ok(UpdateBranchOutcome::MergeFailed {
                base_ref,
                message: stderr,
            }),
        }
    }

    /// Push the AI-resolved merge commit after the user reviewed it.
    /// Wraps `git push origin HEAD` and maps result → outcome.
    pub async fn push_after_review(&self, worktree_path: &str) -> Result<UpdatePullRequestOutcome> {
        let cwd = PathBuf::from(worktree_path);
        let push = time::timeout(
            UPDATE_PUSH_TIMEOUT,
            run_command(&self.git_binary, &["push", "origin", "HEAD"], Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("git push timed out after 60s"))?;
        match push {
            Ok(_) => Ok(UpdatePullRequestOutcome::MergedWithAiResolution),
            Err(err) => Ok(UpdatePullRequestOutcome::PushFailed(err)),
        }
    }

    /// Discard the AI-resolved merge commit (`git reset --hard HEAD~1`)
    /// after the user rejected the review.
    pub async fn discard_after_review(
        &self,
        worktree_path: &str,
    ) -> Result<UpdatePullRequestOutcome> {
        let cwd = PathBuf::from(worktree_path);
        let reset = run_command(&self.git_binary, &["reset", "--hard", "HEAD~1"], Some(&cwd)).await;
        match reset {
            Ok(_) => Ok(UpdatePullRequestOutcome::DiscardedAfterReview),
            Err(err) => Ok(UpdatePullRequestOutcome::DiscardFailed(err)),
        }
    }

    async fn resolve_conflicts_with_ai(
        &self,
        cwd: &Path,
        model: &str,
        base_ref: &str,
        conflicts: &[String],
        progress: Option<mpsc::UnboundedSender<UpdateProgress>>,
    ) -> std::result::Result<(), AiConflictResolutionError> {
        match &self.ai_resolver {
            AiConflictResolver::Direct => {
                self.resolve_conflicts_direct(cwd, model, base_ref, conflicts, progress)
                    .await
            }
            AiConflictResolver::Script(binary) => {
                self.resolve_conflicts_with_script(
                    binary, cwd, model, base_ref, conflicts, progress,
                )
                .await
            }
        }
    }

    async fn resolve_conflicts_with_script(
        &self,
        binary: &Path,
        cwd: &Path,
        model: &str,
        base_ref: &str,
        conflicts: &[String],
        progress: Option<mpsc::UnboundedSender<UpdateProgress>>,
    ) -> std::result::Result<(), AiConflictResolutionError> {
        if !binary_available(binary) {
            return Err(AiConflictResolutionError::Unavailable);
        }

        let prompt_arg = format!("--prompt={}", build_merge_prompt(base_ref, conflicts));
        run_command_streamed(
            binary,
            &[
                "--skip-trust",
                "--yolo",
                "-m",
                model,
                "-o",
                "stream-json",
                &prompt_arg,
            ],
            Some(cwd),
            progress,
        )
        .await
        .map(|_| ())
        .map_err(AiConflictResolutionError::Failed)
    }

    async fn resolve_conflicts_direct(
        &self,
        cwd: &Path,
        model: &str,
        base_ref: &str,
        conflicts: &[String],
        progress: Option<mpsc::UnboundedSender<UpdateProgress>>,
    ) -> std::result::Result<(), AiConflictResolutionError> {
        let client = Client::builder()
            .build()
            .map_err(|err| AiConflictResolutionError::Failed(err.to_string()))?;
        let Some(auth) = gemini_auth() else {
            return self
                .fallback_to_gemini_cli(cwd, model, base_ref, conflicts, progress)
                .await;
        };

        send_ai_activity(
            progress.as_ref(),
            AiActivityEvent::SessionStart {
                model: model.to_string(),
            },
        );

        let system_instruction = build_direct_merge_system_instruction();
        let mut contents = vec![json!({
            "role": "user",
            "parts": [{"text": build_merge_prompt(base_ref, conflicts)}],
        })];
        let mut turn = 0usize;

        loop {
            if turn >= AI_TOOL_LOOP_LIMIT {
                return Err(AiConflictResolutionError::Failed(
                    "Gemini exceeded the conflict-resolution tool loop limit".to_string(),
                ));
            }
            turn += 1;

            let request = build_gemini_request(model, &system_instruction, &contents);
            let turn_started = Instant::now();
            let mut request_builder = client
                .post(format!(
                    "https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse"
                ));
            request_builder = match &auth {
                GeminiAuth::ApiKey(api_key) => request_builder.header("x-goog-api-key", api_key),
            };
            let response = request_builder
                .json(&request)
                .send()
                .await
                .map_err(|err| AiConflictResolutionError::Failed(err.to_string()))?;

            let status = response.status();
            let response = if status.is_success() {
                response
            } else {
                let body = response.text().await.unwrap_or_default();
                return Err(AiConflictResolutionError::Failed(format!(
                    "Gemini API request failed ({status}): {}",
                    extract_gemini_error_message(&body)
                )));
            };

            let stream_result = consume_gemini_stream(response, progress.as_ref(), turn_started)
                .await
                .map_err(AiConflictResolutionError::Failed)?;

            if !stream_result.model_parts.is_empty() {
                contents.push(json!({
                    "role": "model",
                    "parts": stream_result.model_parts,
                }));
            }

            if stream_result.tool_calls.is_empty() {
                return match stream_result.finish_reason.as_deref() {
                    Some("STOP") | None => Ok(()),
                    Some("MAX_TOKENS") => Err(AiConflictResolutionError::Failed(
                        "Gemini truncated its response before finishing conflict resolution"
                            .to_string(),
                    )),
                    Some(reason) => Err(AiConflictResolutionError::Failed(format!(
                        "Gemini stopped with finish reason {reason}"
                    ))),
                };
            }

            let tool_response_parts = self
                .execute_direct_tool_calls(cwd, &stream_result.tool_calls, progress.as_ref())
                .await;
            contents.push(json!({
                "role": "user",
                "parts": tool_response_parts,
            }));
        }
    }

    async fn execute_direct_tool_calls(
        &self,
        cwd: &Path,
        calls: &[GeminiFunctionCall],
        progress: Option<&mpsc::UnboundedSender<UpdateProgress>>,
    ) -> Vec<Value> {
        let mut responses = Vec::with_capacity(calls.len());
        for call in calls {
            send_ai_activity(
                progress,
                AiActivityEvent::ToolCall {
                    tool_name: call.name.clone(),
                    summary: summarize_direct_tool_call(call),
                },
            );

            let result = execute_direct_tool_call(cwd, call).await;
            let (status, content) = match result {
                Ok(output) => {
                    send_ai_activity(
                        progress,
                        AiActivityEvent::ToolResult {
                            tool_name: Some(call.name.clone()),
                            status: AiToolResultStatus::Success,
                            detail: clip_activity_text(&output.summary),
                        },
                    );
                    (json!(true), output.model_response)
                }
                Err(err) => {
                    send_ai_activity(
                        progress,
                        AiActivityEvent::ToolResult {
                            tool_name: Some(call.name.clone()),
                            status: AiToolResultStatus::Error,
                            detail: clip_activity_text(&err),
                        },
                    );
                    (json!(false), err)
                }
            };

            responses.push(json!({
                "functionResponse": {
                    "name": call.name,
                    "response": {
                        "name": call.name,
                        "ok": status,
                        "content": content,
                    }
                }
            }));
        }
        responses
    }

    async fn fallback_to_gemini_cli(
        &self,
        cwd: &Path,
        model: &str,
        base_ref: &str,
        conflicts: &[String],
        progress: Option<mpsc::UnboundedSender<UpdateProgress>>,
    ) -> std::result::Result<(), AiConflictResolutionError> {
        let binary = Path::new(GEMINI_CLI_BINARY);
        if !binary_available(binary) {
            return Err(AiConflictResolutionError::Unavailable);
        }
        send_ai_activity(
            progress.as_ref(),
            AiActivityEvent::Notice {
                severity: AiActivitySeverity::Info,
                message: "No direct Gemini auth found; falling back to gemini CLI.".to_string(),
            },
        );
        self.resolve_conflicts_with_script(binary, cwd, model, base_ref, conflicts, progress)
            .await
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
        let upstream = resolve_base_ref_with_binary(&self.git_binary, cwd).await?;
        let result = time::timeout(
            COMMAND_TIMEOUT,
            run_command(
                &self.git_binary,
                &["diff", "--shortstat", &upstream],
                Some(cwd),
            ),
        )
        .await
        .ok()?;
        let Ok(output) = result else { return None };
        let (insertions, deletions) = parse_shortstat(&output);
        Some(BranchStatus {
            ahead: insertions,
            behind: deletions,
            upstream_branch: Some(upstream),
        })
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
    ///
    /// `on_cycle` is decided by the watch loop based on `next_pr_fetch_at`:
    /// `true` once per refresh period, `false` between periods. Off-cycle
    /// runs still pick up brand-new branches and SHA changes so the UI
    /// keeps up with local commits without disturbing the cycle rhythm.
    async fn refresh_pull_requests(&self, rows: &[DashboardRow], on_cycle: bool) {
        if !self.pr_enrichment_enabled() || self.is_rate_limited() {
            return;
        }

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
                        Some(entry) => entry.sha != sha || on_cycle,
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
                for (branch, sha) in &to_fetch {
                    let pr = results.get(branch).cloned().flatten();
                    state.entries.insert(
                        branch.clone(),
                        PrCacheEntry {
                            sha: sha.clone(),
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
            "b{i}: pullRequests(headRefName: \"{}\", states: [OPEN, CLOSED, MERGED], first: 1, orderBy: {{field: CREATED_AT, direction: DESC}}) {{ nodes {{ number url title state isDraft mergeStateStatus reviewDecision reviewRequests(first: 100) {{ totalCount nodes {{ requestedReviewer {{ __typename ... on User {{ login }} }} }} }} latestOpinionatedReviews(first: 100) {{ nodes {{ state author {{ login }} }} }} latestReviews(first: 100) {{ nodes {{ state author {{ login }} }} }} commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state contexts(first: 100) {{ nodes {{ __typename ... on CheckRun {{ status conclusion }} ... on StatusContext {{ state }} }} }} }} }} }} }} }} }} ",
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
                PullRequest {
                    number: node.number,
                    state,
                    url: node.url,
                    title: node.title,
                    checks_status,
                    review_status,
                    merge_status,
                    reviewers,
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
#[derive(Deserialize)]
struct GhNode {
    number: u64,
    state: String,
    url: String,
    title: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    #[serde(rename = "mergeStateStatus", default)]
    merge_state_status: Option<String>,
    #[serde(rename = "reviewDecision", default)]
    review_decision: Option<String>,
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

/// Parse `git diff --shortstat` output and return `(insertions, deletions)`.
/// Sample inputs:
///   ` 28 files changed, 2815 insertions(+), 42 deletions(-)`
///   ` 1 file changed, 5 insertions(+)`
///   `` (no diff → both zero)
/// Classify the stdout of `git merge <ref>` into the corresponding
/// `UpdateBranchOutcome` variant so the dashboard toast can describe
/// what actually happened. Git's output is stable enough to key on:
/// - "Already up to date." for the no-op case,
/// - a line containing "Fast-forward" for the fast-forward case,
/// - everything else (including "Merge made by ...") is a merge commit.
///
/// `summary` is the first non-empty line of stdout — short enough for
/// the toast and informative enough to anchor what just happened.
fn classify_merge_output(base_ref: String, stdout: &str) -> UpdateBranchOutcome {
    let trimmed = stdout.trim();
    if trimmed.starts_with("Already up to date") || trimmed.starts_with("Already up-to-date") {
        return UpdateBranchOutcome::AlreadyUpToDate { base_ref };
    }
    let summary = trimmed
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string();
    if trimmed.lines().any(|line| line.trim() == "Fast-forward") {
        UpdateBranchOutcome::FastForwarded { base_ref, summary }
    } else {
        UpdateBranchOutcome::Merged { base_ref, summary }
    }
}

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

/// Rejects a `write_file` call that would replace a still-conflicted file
/// with a catastrophically smaller payload. The file is considered
/// in-conflict when the on-disk content still has merge markers, and the
/// new content is "catastrophically smaller" when it's under 10 % of the
/// existing size (with a 200-byte floor so trivial files don't trip the
/// guard). The returned error is fed back to the model as the tool
/// response so it can retry within its remaining turns instead of the
/// outer pipeline only catching the destruction post-hoc.
fn guard_destructive_overwrite(
    existing: &str,
    new_content: &str,
) -> std::result::Result<(), String> {
    let has_markers = existing.contains("<<<<<<<")
        || existing.contains("=======")
        || existing.contains(">>>>>>>");
    if !has_markers {
        return Ok(());
    }
    if existing.len() < 200 {
        return Ok(());
    }
    if new_content.len() * 10 >= existing.len() {
        return Ok(());
    }
    Err(format!(
        "this file is in merge-conflict state (it still has conflict markers) \
         and the proposed content ({} bytes) is catastrophically smaller than \
         the existing file ({} bytes). A real merge must preserve content from \
         both sides — read the file in full, combine both sides' intent, and \
         write the merged result. If you cannot merge this file safely, do not \
         call write_file on this path; leave the markers in place and explain \
         in your final reply.",
        new_content.len(),
        existing.len()
    ))
}

/// Returns the path of the first file that looks catastrophically truncated
/// after AI conflict resolution. A file is considered truncated when its
/// resolved size is less than 10 % of the larger pre-merge side, provided
/// that larger side was at least 100 bytes. This catches the failure mode
/// where an AI writes a placeholder word (e.g. "resolved") instead of
/// properly merging the content.
async fn catastrophically_truncated(
    git: &Path,
    cwd: &Path,
    conflicts: &[String],
) -> Option<String> {
    for file in conflicts {
        let ours = run_command(git, &["show", &format!(":2:{file}")], Some(cwd))
            .await
            .unwrap_or_default()
            .len();
        let theirs = run_command(git, &["show", &format!(":3:{file}")], Some(cwd))
            .await
            .unwrap_or_default()
            .len();
        let baseline = ours.max(theirs);
        if baseline < 100 {
            continue;
        }
        let resolved = tokio::fs::metadata(cwd.join(file))
            .await
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        if resolved < baseline / 10 {
            return Some(file.clone());
        }
    }
    None
}

#[derive(Debug, Default)]
struct GeminiTurnResult {
    model_parts: Vec<Value>,
    tool_calls: Vec<GeminiFunctionCall>,
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GeminiFunctionCall {
    name: String,
    #[serde(default)]
    args: Value,
}

#[derive(Debug, Deserialize)]
struct GeminiUsageMetadata {
    #[serde(rename = "totalTokenCount")]
    total_token_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GeminiResponsePart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(rename = "functionCall", default)]
    function_call: Option<GeminiFunctionCall>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidateContent {
    #[serde(default)]
    parts: Vec<GeminiResponsePart>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiCandidateContent>,
    #[serde(rename = "finishReason", default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiStreamChunk {
    #[serde(default)]
    candidates: Option<Vec<GeminiCandidate>>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug)]
struct DirectToolOutput {
    summary: String,
    model_response: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GeminiAuth {
    ApiKey(String),
}

#[derive(Debug, Deserialize)]
struct GeminiCliSettingsFile {
    #[serde(default)]
    security: Option<GeminiCliSecurity>,
}

#[derive(Debug, Deserialize)]
struct GeminiCliSecurity {
    #[serde(default)]
    auth: Option<GeminiCliSecurityAuth>,
}

#[derive(Debug, Deserialize)]
struct GeminiCliSecurityAuth {
    #[serde(rename = "selectedType", default)]
    selected_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct StageFileArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
struct ShellCommandArgs {
    command: String,
}

fn gemini_api_key() -> Option<String> {
    ["GEMINI_API_KEY", "GOOGLE_API_KEY"]
        .into_iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
}

fn gemini_cli_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home).join(".gemini");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gemini")
}

fn gemini_cli_settings_path() -> PathBuf {
    gemini_cli_dir().join("settings.json")
}

fn gemini_cli_oauth_creds_path() -> PathBuf {
    gemini_cli_dir().join("oauth_creds.json")
}

fn gemini_auth() -> Option<GeminiAuth> {
    if let Some(api_key) = gemini_api_key() {
        return Some(GeminiAuth::ApiKey(api_key));
    }

    let settings = load_gemini_cli_settings().ok()?;
    let selected_type = selected_gemini_cli_auth_type(&settings).unwrap_or_default();
    if selected_type == "oauth-personal" && gemini_cli_oauth_creds_path().exists() {
        return None;
    }
    None
}

fn selected_gemini_cli_auth_type(settings: &GeminiCliSettingsFile) -> Option<String> {
    settings
        .security
        .as_ref()
        .and_then(|security| security.auth.as_ref())
        .and_then(|auth| auth.selected_type.clone())
}

fn load_gemini_cli_settings() -> std::result::Result<GeminiCliSettingsFile, String> {
    let raw = fs::read_to_string(gemini_cli_settings_path()).map_err(|err| err.to_string())?;
    serde_json::from_str(&raw).map_err(|err| err.to_string())
}

fn extract_gemini_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| clip_activity_text(body))
}

fn send_ai_activity(
    progress: Option<&mpsc::UnboundedSender<UpdateProgress>>,
    event: AiActivityEvent,
) {
    if let Some(tx) = progress {
        let _ = tx.send(UpdateProgress::AiOutput(event));
    }
}

fn build_direct_merge_system_instruction() -> String {
    [
        "You are operating inside an automated merge-conflict resolver.",
        "Available tools:",
        "- read_file(path): read a UTF-8 file from the repository.",
        "- write_file(path, content): overwrite a UTF-8 file with exact content.",
        "- stage_file(path): stage exactly one file with git add -- <path>.",
        "- run_shell_command(command): run a non-interactive shell command in the repository root.",
        "Prefer read_file + write_file for code changes. Use run_shell_command for git inspection, fast checks, grep-like search, and targeted tests.",
        "Forbidden git state commands (fetch, pull, merge, reset, checkout, commit, push) will be rejected by the harness.",
        "The outer pipeline will do the final git add/commit after you finish, so never ask for confirmation.",
    ]
    .join("\n")
}

fn build_gemini_request(model: &str, system_instruction: &str, contents: &[Value]) -> Value {
    let thinking_config = if model.starts_with("gemini-2.5") {
        json!({
            "includeThoughts": true,
            "thinkingBudget": 8192,
        })
    } else {
        json!({
            "includeThoughts": true,
            "thinkingLevel": "HIGH",
        })
    };

    json!({
        "systemInstruction": {
            "parts": [{"text": system_instruction}],
        },
        "contents": contents,
        "tools": [{
            "functionDeclarations": [
                {
                    "name": "read_file",
                    "description": "Read a UTF-8 file from the repository.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Repository-relative file path."}
                        },
                        "required": ["path"]
                    }
                },
                {
                    "name": "write_file",
                    "description": "Overwrite a UTF-8 file in the repository with exact content.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Repository-relative file path."},
                            "content": {"type": "string", "description": "Full replacement file content."}
                        },
                        "required": ["path", "content"]
                    }
                },
                {
                    "name": "stage_file",
                    "description": "Stage exactly one file with git add -- <path>.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Repository-relative file path to stage."}
                        },
                        "required": ["path"]
                    }
                },
                {
                    "name": "run_shell_command",
                    "description": "Run a non-interactive shell command in the repository root. Useful for git inspection, search, and targeted tests.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": {"type": "string", "description": "Shell command to run."}
                        },
                        "required": ["command"]
                    }
                }
            ]
        }],
        "toolConfig": {
            "functionCallingConfig": {
                "mode": "AUTO"
            }
        },
        "generationConfig": {
            "temperature": 0,
            "topP": 1,
            "maxOutputTokens": 32768,
            "thinkingConfig": thinking_config,
        }
    })
}

async fn consume_gemini_stream(
    mut response: reqwest::Response,
    progress: Option<&mpsc::UnboundedSender<UpdateProgress>>,
    started_at: Instant,
) -> std::result::Result<GeminiTurnResult, String> {
    let mut turn = GeminiTurnResult::default();
    let mut buffer = String::new();

    while let Some(chunk) = response.chunk().await.map_err(|err| err.to_string())? {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(index) = buffer.find("\n\n") {
            let event = buffer[..index].to_string();
            buffer.drain(..index + 2);
            apply_gemini_sse_event(&event, &mut turn, progress, started_at)?;
        }
    }

    if !buffer.trim().is_empty() {
        apply_gemini_sse_event(buffer.trim_end(), &mut turn, progress, started_at)?;
    }

    Ok(turn)
}

fn apply_gemini_sse_event(
    event: &str,
    turn: &mut GeminiTurnResult,
    progress: Option<&mpsc::UnboundedSender<UpdateProgress>>,
    started_at: Instant,
) -> std::result::Result<(), String> {
    let data = event
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() || data == "[DONE]" {
        return Ok(());
    }

    let chunk: GeminiStreamChunk = serde_json::from_str(&data)
        .map_err(|err| format!("invalid Gemini stream event: {err}: {data}"))?;
    let candidate = chunk
        .candidates
        .as_ref()
        .and_then(|candidates| candidates.first());

    if let Some(candidate) = candidate {
        if let Some(content) = candidate.content.as_ref() {
            for part in &content.parts {
                if let Some(function_call) = part.function_call.as_ref() {
                    turn.tool_calls.push(function_call.clone());
                    turn.model_parts.push(json!({
                        "functionCall": {
                            "name": function_call.name,
                            "args": function_call.args,
                        }
                    }));
                    continue;
                }

                let Some(text) = part.text.as_deref() else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }

                if part.thought.unwrap_or(false) {
                    send_ai_activity(
                        progress,
                        AiActivityEvent::Thinking {
                            content: text.to_string(),
                        },
                    );
                } else {
                    send_ai_activity(
                        progress,
                        AiActivityEvent::AssistantText {
                            content: text.to_string(),
                        },
                    );
                }
                append_model_text_part(&mut turn.model_parts, text, part.thought.unwrap_or(false));
            }
        }

        if let Some(reason) = candidate.finish_reason.as_deref() {
            turn.finish_reason = Some(reason.to_string());
            if let Some(usage) = chunk.usage_metadata.as_ref() {
                send_ai_activity(
                    progress,
                    AiActivityEvent::Summary {
                        tool_calls: turn.tool_calls.len() as u64,
                        duration_ms: started_at.elapsed().as_millis() as u64,
                        total_tokens: usage.total_token_count.unwrap_or(0),
                    },
                );
            }
        }
    }

    Ok(())
}

fn append_model_text_part(parts: &mut Vec<Value>, text: &str, thought: bool) {
    if let Some(last) = parts.last_mut() {
        if last.get("functionCall").is_none() {
            let same_thought = last
                .get("thought")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
                == thought;
            if same_thought {
                if let Some(existing_value) = last.get_mut("text") {
                    let existing_text = existing_value.as_str().unwrap_or_default();
                    let merged = format!("{existing_text}{text}");
                    *existing_value = Value::String(merged);
                    return;
                }
            }
        }
    }

    if thought {
        parts.push(json!({"text": text, "thought": true}));
    } else {
        parts.push(json!({"text": text}));
    }
}

fn summarize_direct_tool_call(call: &GeminiFunctionCall) -> String {
    match call.name.as_str() {
        "read_file" | "write_file" | "stage_file" => call
            .args
            .get("path")
            .and_then(|value| value.as_str())
            .map(clip_activity_text)
            .unwrap_or_default(),
        "run_shell_command" => call
            .args
            .get("command")
            .and_then(|value| value.as_str())
            .map(clip_activity_text)
            .unwrap_or_default(),
        _ => clip_activity_text(&call.args.to_string()),
    }
}

async fn execute_direct_tool_call(
    cwd: &Path,
    call: &GeminiFunctionCall,
) -> std::result::Result<DirectToolOutput, String> {
    match call.name.as_str() {
        "read_file" => {
            let args: ReadFileArgs = serde_json::from_value(call.args.clone())
                .map_err(|err| format!("invalid read_file arguments: {err}"))?;
            let path = resolve_repo_path(cwd, &args.path)?;
            let display = display_repo_path(cwd, &path);
            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|err| format!("failed to read {display}: {err}"))?;
            Ok(DirectToolOutput {
                summary: format!("{} bytes", content.len()),
                model_response: truncate_tool_output_for_model(&format!(
                    "FILE: {display}\n{content}"
                )),
            })
        }
        "write_file" => {
            let args: WriteFileArgs = serde_json::from_value(call.args.clone())
                .map_err(|err| format!("invalid write_file arguments: {err}"))?;
            let path = resolve_repo_path(cwd, &args.path)?;
            let display = display_repo_path(cwd, &path);
            let existing = tokio::fs::read_to_string(&path).await.unwrap_or_default();
            if let Err(reason) = guard_destructive_overwrite(&existing, &args.content) {
                return Err(format!("refusing to write {display}: {reason}"));
            }
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|err| format!("failed to prepare {display}: {err}"))?;
            }
            tokio::fs::write(&path, args.content.as_bytes())
                .await
                .map_err(|err| format!("failed to write {display}: {err}"))?;
            Ok(DirectToolOutput {
                summary: format!("wrote {display}"),
                model_response: format!("WROTE: {display}"),
            })
        }
        "stage_file" => {
            let args: StageFileArgs = serde_json::from_value(call.args.clone())
                .map_err(|err| format!("invalid stage_file arguments: {err}"))?;
            let path = resolve_repo_path(cwd, &args.path)?;
            let display = display_repo_path(cwd, &path);
            run_command(Path::new("git"), &["add", "--", &display], Some(cwd))
                .await
                .map_err(|err| format!("failed to stage {display}: {err}"))?;
            Ok(DirectToolOutput {
                summary: format!("staged {display}"),
                model_response: format!("STAGED: {display}"),
            })
        }
        "run_shell_command" => {
            let args: ShellCommandArgs = serde_json::from_value(call.args.clone())
                .map_err(|err| format!("invalid run_shell_command arguments: {err}"))?;
            validate_shell_command(&args.command)?;
            run_shell_command_tool(cwd, &args.command).await
        }
        other => Err(format!("unknown Gemini tool `{other}`")),
    }
}

fn resolve_repo_path(cwd: &Path, raw: &str) -> std::result::Result<PathBuf, String> {
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        cwd.join(raw)
    };
    let normalized = normalize_path(&candidate);
    if !normalized.starts_with(cwd) {
        return Err(format!("path escapes repository: {raw}"));
    }
    if normalized
        .components()
        .any(|component| component.as_os_str() == ".git")
    {
        return Err(format!("refusing to touch .git internals: {raw}"));
    }
    Ok(normalized)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn display_repo_path(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn truncate_tool_output_for_model(output: &str) -> String {
    if output.len() <= AI_TOOL_OUTPUT_LIMIT {
        return output.to_string();
    }
    let mut clipped = output
        .chars()
        .take(AI_TOOL_OUTPUT_LIMIT)
        .collect::<String>();
    clipped.push_str("\n\n[truncated by wisetree]");
    clipped
}

fn validate_shell_command(command: &str) -> std::result::Result<(), String> {
    let tokens = command
        .split_whitespace()
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>();
    for (index, token) in tokens.iter().enumerate() {
        if token != "git" {
            continue;
        }
        let Some(subcommand) = tokens.get(index + 1) else {
            continue;
        };
        match subcommand.as_str() {
            "fetch" | "pull" | "merge" | "reset" | "checkout" | "commit" | "push" => {
                return Err(format!(
                    "forbidden command in run_shell_command: git {subcommand}"
                ));
            }
            "add" => {
                if let Some(arg) = tokens.get(index + 2) {
                    if matches!(arg.as_str(), "." | "-a" | "-A" | "--all") {
                        return Err(
                            "forbidden command in run_shell_command: git add must target an explicit file"
                                .to_string(),
                        );
                    }
                }
            }
            _ => {}
        }
    }
    if command.contains("/.git") || command.contains(".git/") {
        return Err("forbidden path in run_shell_command: .git internals".to_string());
    }
    Ok(())
}

async fn run_shell_command_tool(
    cwd: &Path,
    command: &str,
) -> std::result::Result<DirectToolOutput, String> {
    let mut cmd = Command::new("sh");
    cmd.arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = time::timeout(AI_TOOL_TIMEOUT, cmd.output())
        .await
        .map_err(|_| format!("command timed out after {}s", AI_TOOL_TIMEOUT.as_secs()))?
        .map_err(|err| err.to_string())?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let rendered = format_shell_result(command, output.status.code(), &stdout, &stderr);
    if output.status.success() {
        Ok(DirectToolOutput {
            summary: first_non_empty_line(&stdout)
                .or_else(|| first_non_empty_line(&stderr))
                .unwrap_or_else(|| "command succeeded".to_string()),
            model_response: truncate_tool_output_for_model(&rendered),
        })
    } else {
        Err(truncate_tool_output_for_model(&rendered))
    }
}

fn format_shell_result(command: &str, code: Option<i32>, stdout: &str, stderr: &str) -> String {
    let mut sections = vec![format!("COMMAND: {command}")];
    if let Some(code) = code {
        sections.push(format!("EXIT CODE: {code}"));
    }
    if !stdout.is_empty() {
        sections.push(format!("STDOUT:\n{stdout}"));
    }
    if !stderr.is_empty() {
        sections.push(format!("STDERR:\n{stderr}"));
    }
    sections.join("\n\n")
}

fn first_non_empty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// Spawn a subprocess and forward every stdout/stderr line through
/// `progress` (as `UpdateProgress::AiOutput`) as it arrives. Returns the
/// same `Ok(stdout) / Err(stderr)` shape as `run_command` once the
/// process exits so callers can keep their post-processing logic unchanged.
/// Used to give the user a live view of the Gemini CLI's streamed activity
/// during conflict resolution (tool calls, assistant text, warnings, and any
/// future thinking events upstream chooses to expose).
async fn run_command_streamed(
    binary: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    progress: Option<mpsc::UnboundedSender<UpdateProgress>>,
) -> std::result::Result<String, String> {
    let mut cmd = Command::new(binary);
    cmd.args(args)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }

    let mut child = cmd.spawn().map_err(|err| err.to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "child stdout unavailable".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "child stderr unavailable".to_string())?;

    let stdout_buf = Arc::new(Mutex::new(String::new()));
    let stderr_buf = Arc::new(Mutex::new(String::new()));

    let stdout_task = tokio::spawn(forward_stream(
        BufReader::new(stdout),
        Arc::clone(&stdout_buf),
        progress.clone(),
    ));
    let stderr_task = tokio::spawn(forward_stream(
        BufReader::new(stderr),
        Arc::clone(&stderr_buf),
        progress.clone(),
    ));

    let status = child.wait().await.map_err(|err| err.to_string())?;
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let stdout_str = stdout_buf
        .lock()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let stderr_str = stderr_buf
        .lock()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    if status.success() {
        Ok(stdout_str)
    } else if stderr_str.is_empty() {
        Err(format!("exit status: {status}"))
    } else {
        Err(stderr_str)
    }
}

/// Read `reader` line-by-line, append each (raw) line to `buf`, and
/// forward an ANSI-stripped copy through `progress`. Lines without a
/// trailing newline (the final partial line) are flushed when EOF hits.
///
/// Gemini's `-o stream-json` mode emits one NDJSON event per line
/// (`init` / `message` / `tool_use` / `tool_result` / `error` / `result`);
/// those are translated into structured UI events. Non-JSON lines (the four
/// startup warnings Gemini prints on stderr before switching to structured
/// output) pass through verbatim so the user still sees them.
async fn forward_stream<R>(
    mut reader: BufReader<R>,
    buf: Arc<Mutex<String>>,
    progress: Option<mpsc::UnboundedSender<UpdateProgress>>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(mut guard) = buf.lock() {
                    guard.push_str(&line);
                }
                if let Some(tx) = progress.as_ref() {
                    let clean = strip_ansi(line.trim_end_matches(['\r', '\n']));
                    if let Some(formatted) = format_stream_event(&clean) {
                        let _ = tx.send(UpdateProgress::AiOutput(formatted));
                    }
                }
            }
            Err(_) => break,
        }
    }
}

/// Translate a single line of Gemini stdout/stderr into a structured event for
/// the AI Activity panel. Returns `None` for lines we deliberately hide
/// (empties, the user-prompt echo). Lines that don't parse as a known NDJSON
/// event are surfaced as raw text so startup warnings still reach the user.
fn format_stream_event(line: &str) -> Option<AiActivityEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => {
            return Some(AiActivityEvent::Raw {
                text: clip_activity_text(line),
            });
        }
    };
    let event_type = value.get("type").and_then(|v| v.as_str())?;
    match event_type {
        "init" => {
            let model = value.get("model").and_then(|v| v.as_str()).unwrap_or("?");
            Some(AiActivityEvent::SessionStart {
                model: model.to_string(),
            })
        }
        "message" => {
            let role = value.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role != "assistant" {
                return None;
            }
            let content = value.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.is_empty() {
                return None;
            }
            Some(AiActivityEvent::AssistantText {
                content: flatten_stream_chunk(content),
            })
        }
        "thought" | "thinking" => {
            let content = json_str(&value, &["content"])
                .or_else(|| json_str(&value, &["text"]))
                .or_else(|| json_str(&value, &["thought"]))
                .or_else(|| json_str(&value, &["summary"]))
                .or_else(|| json_str(&value, &["subject"]))
                .unwrap_or("");
            if content.is_empty() {
                return None;
            }
            Some(AiActivityEvent::Thinking {
                content: flatten_stream_chunk(content),
            })
        }
        "tool_use" => {
            let tool = value
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let summary = summarize_tool_params(value.get("parameters"));
            Some(AiActivityEvent::ToolCall {
                tool_name: tool.to_string(),
                summary,
            })
        }
        "tool_result" => {
            let status = value
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("success");
            let tool_name = value
                .get("tool_id")
                .and_then(|v| v.as_str())
                .and_then(tool_name_from_tool_id);
            let detail = summarize_tool_result_detail(&value);
            if status == "success" {
                return detail.map(|detail| AiActivityEvent::ToolResult {
                    tool_name,
                    status: AiToolResultStatus::Success,
                    detail,
                });
            }
            Some(AiActivityEvent::ToolResult {
                tool_name,
                status: AiToolResultStatus::Error,
                detail: detail.unwrap_or_else(|| "failed".to_string()),
            })
        }
        "error" => {
            let message = value.get("message").and_then(|v| v.as_str()).unwrap_or("");
            if message.is_empty() {
                return None;
            }
            let severity = match value
                .get("severity")
                .and_then(|v| v.as_str())
                .unwrap_or("error")
            {
                "warning" => AiActivitySeverity::Warning,
                "info" => AiActivitySeverity::Info,
                _ => AiActivitySeverity::Error,
            };
            Some(AiActivityEvent::Notice {
                severity,
                message: clip_activity_text(message),
            })
        }
        "result" => {
            let status = value
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("success");
            if status == "error" {
                let message = json_str(&value, &["error", "message"])
                    .or_else(|| value.get("message").and_then(|v| v.as_str()))
                    .unwrap_or("request failed");
                return Some(AiActivityEvent::Notice {
                    severity: AiActivitySeverity::Error,
                    message: clip_activity_text(message),
                });
            }
            let stats = value.get("stats");
            let tool_calls = stats
                .and_then(|s| s.get("tool_calls"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let duration_ms = stats
                .and_then(|s| s.get("duration_ms"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let tokens = stats
                .and_then(|s| s.get("total_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(AiActivityEvent::Summary {
                tool_calls,
                duration_ms,
                total_tokens: tokens,
            })
        }
        other => {
            if other.contains("thought") || other.contains("thinking") {
                let content = json_str(&value, &["content"])
                    .or_else(|| json_str(&value, &["text"]))
                    .or_else(|| json_str(&value, &["summary"]))
                    .unwrap_or("");
                if !content.is_empty() {
                    return Some(AiActivityEvent::Thinking {
                        content: flatten_stream_chunk(content),
                    });
                }
            }
            let text = value
                .get("message")
                .and_then(|v| v.as_str())
                .or_else(|| value.get("content").and_then(|v| v.as_str()))
                .or_else(|| value.get("text").and_then(|v| v.as_str()));
            match text {
                Some(text) if !text.is_empty() => Some(AiActivityEvent::Raw {
                    text: format!("[{other}] {}", clip_activity_text(text)),
                }),
                _ => Some(AiActivityEvent::Raw {
                    text: clip_activity_text(line),
                }),
            }
        }
    }
}

fn json_str<'a>(value: &'a serde_json::Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_str()
}

fn flatten_stream_chunk(input: &str) -> String {
    input
        .chars()
        .map(|ch| if matches!(ch, '\r' | '\n') { ' ' } else { ch })
        .collect()
}

fn collapse_activity_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip_activity_text(input: &str) -> String {
    const MAX_CHARS: usize = 160;
    let collapsed = collapse_activity_whitespace(input);
    let total = collapsed.chars().count();
    if total <= MAX_CHARS {
        return collapsed;
    }
    let prefix: String = collapsed.chars().take(MAX_CHARS - 3).collect();
    format!("{prefix}...")
}

fn summarize_tool_result_detail(value: &serde_json::Value) -> Option<String> {
    json_str(value, &["error", "message"])
        .or_else(|| value.get("output").and_then(|v| v.as_str()))
        .map(clip_activity_text)
        .filter(|detail| !detail.is_empty())
}

fn tool_name_from_tool_id(tool_id: &str) -> Option<String> {
    let tool_name = tool_id.split("__").next().unwrap_or(tool_id).trim();
    if tool_name.is_empty() {
        None
    } else {
        Some(tool_name.to_string())
    }
}

/// Pick the single most informative key in a tool-call parameter blob.
/// Gemini's tools all carry one obvious "what is this about" field
/// (`file_path` for read/edit, `command` for shell, `pattern` for
/// grep, …); rendering just that keeps the activity row short. Falls
/// back to the first key=value pair when nothing recognisable is
/// found.
fn summarize_tool_params(params: Option<&serde_json::Value>) -> String {
    let Some(obj) = params.and_then(|v| v.as_object()) else {
        return String::new();
    };
    const PREFERRED: &[&str] = &[
        "file_path",
        "path",
        "absolute_path",
        "command",
        "pattern",
        "query",
        "url",
        "strategic_intent",
    ];
    for key in PREFERRED {
        if let Some(v) = obj.get(*key).and_then(|v| v.as_str()) {
            return clip_activity_text(v);
        }
    }
    obj.iter()
        .next()
        .map(|(k, v)| match v.as_str() {
            Some(s) => clip_activity_text(&format!("{k}={s}")),
            None => clip_activity_text(&format!("{k}={v}")),
        })
        .unwrap_or_default()
}

/// Strip CSI / OSC ANSI escape sequences and bare ESC characters so
/// streamed AI output renders cleanly in a TUI Paragraph (which doesn't
/// interpret escapes). Intentionally minimal — handles the
/// `ESC [ ... <final>` and `ESC ] ... BEL` forms that account for
/// virtually all color/cursor output. UTF-8 safe.
fn strip_ansi(input: &str) -> String {
    const ESC: char = '\x1b';
    const BEL: char = '\x07';
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != ESC {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // Consume params then the final byte (range 0x40..=0x7E).
                while let Some(&next) = chars.peek() {
                    chars.next();
                    let code = next as u32;
                    if (0x40..=0x7e).contains(&code) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // OSC: terminated by BEL or ESC \.
                while let Some(&next) = chars.peek() {
                    if next == BEL {
                        chars.next();
                        break;
                    }
                    if next == ESC {
                        chars.next();
                        if let Some('\\') = chars.peek() {
                            chars.next();
                        }
                        break;
                    }
                    chars.next();
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

/// Return the first reachable base ref in `BASE_REF_PRIORITY`. Probes each
/// ref with `git rev-parse --verify` against the supplied worktree. Used
/// by both the dashboard's behind probe and the "Update Pull Request"
/// flow so the priority order can never drift.
pub async fn resolve_base_ref(cwd: &Path) -> Option<String> {
    resolve_base_ref_with_binary(Path::new("git"), cwd).await
}

pub(crate) async fn resolve_base_ref_with_binary(git_binary: &Path, cwd: &Path) -> Option<String> {
    for candidate in BASE_REF_PRIORITY {
        let result = time::timeout(
            COMMAND_TIMEOUT,
            run_command(
                git_binary,
                &["rev-parse", "--verify", "--quiet", candidate],
                Some(cwd),
            ),
        )
        .await
        .ok()?;
        if result.is_ok() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// True when the row's branch is behind its base — either the PR's
/// `merge_status` reports `Behind`, or git's local ahead/behind shows
/// `behind > 0`. Single source of truth for the "Update Pull Request"
/// menu-visibility rule and tests.
pub fn is_behind(row: &DashboardRow) -> bool {
    let merge_says_behind = row
        .pull_request
        .as_ref()
        .and_then(|pr| pr.merge_status)
        .map(|status| matches!(status, MergeStatus::Behind))
        .unwrap_or(false);
    let git_says_behind = row
        .worktree
        .branch_status
        .as_ref()
        .map(|status| status.behind > 0)
        .unwrap_or(false);
    merge_says_behind || git_says_behind
}

/// Count of commits HEAD is behind `base_ref`. `None` when the count
/// can't be produced (ref missing, parse error, …). Used by the update
/// pipeline to short-circuit `AlreadyUpToDate` after a fetch.
async fn behind_against_base(git_binary: &Path, cwd: &Path, base_ref: &str) -> Option<u64> {
    let spec = format!("HEAD...{base_ref}");
    let output = run_command(
        git_binary,
        &["rev-list", "--left-right", "--count", &spec],
        Some(cwd),
    )
    .await
    .ok()?;
    let mut parts = output.split_whitespace();
    let _ahead = parts.next()?;
    let behind = parts.next()?;
    behind.parse::<u64>().ok()
}

/// Return the list of files currently in conflict (`UU`, `AA`, etc.).
/// Empty when there are no conflicts.
async fn conflicted_files(git_binary: &Path, cwd: &Path) -> Vec<String> {
    match run_command(
        git_binary,
        &["diff", "--name-only", "--diff-filter=U"],
        Some(cwd),
    )
    .await
    {
        Ok(out) => out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The task prompt fed to Gemini when resolving merge conflicts. The body of
/// `prompts/merger.md` is embedded at compile time so the binary has no runtime
/// dependency on the prompt file.
/// `MERGE_REF` is substituted with the resolved base ref; `CONFLICTED_FILES`
/// is substituted with the bulleted list of unmerged paths produced by
/// `git diff --name-only --diff-filter=U`.
///
/// The prompt deliberately ships **without** YAML frontmatter so it stays a
/// plain instruction body no matter which Gemini harness executes it.
fn build_merge_prompt(base_ref: &str, conflicts: &[String]) -> String {
    const MERGER_PROMPT: &str = include_str!("../../prompts/merger.md");
    let bulleted = if conflicts.is_empty() {
        "  (none reported — re-run `git diff --name-only --diff-filter=U`)".to_string()
    } else {
        conflicts
            .iter()
            .map(|path| format!("  - {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    MERGER_PROMPT
        .replace("MERGE_REF", base_ref)
        .replace("CONFLICTED_FILES", &bulleted)
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
    fn guard_rejects_placeholder_write_to_conflicted_file() {
        let existing = format!(
            "{}\n<<<<<<< HEAD\nfrom ours\n=======\nfrom theirs\n>>>>>>> base\n{}\n",
            "x".repeat(500),
            "y".repeat(500),
        );
        let err = guard_destructive_overwrite(&existing, "resolved")
            .expect_err("placeholder must be rejected when conflict markers are present");
        assert!(
            err.contains("merge-conflict"),
            "unexpected guard message: {err}"
        );
    }

    #[test]
    fn guard_allows_real_merge_to_conflicted_file() {
        let existing = format!(
            "{}\n<<<<<<< HEAD\nfrom ours\n=======\nfrom theirs\n>>>>>>> base\n{}\n",
            "x".repeat(500),
            "y".repeat(500),
        );
        let merged = format!(
            "{}\nmerged body line\n{}\n",
            "x".repeat(500),
            "y".repeat(500)
        );
        assert!(guard_destructive_overwrite(&existing, &merged).is_ok());
    }

    #[test]
    fn guard_allows_shrinking_write_when_no_conflict_markers() {
        // No markers means the file isn't in a conflicted state — a deliberate
        // truncation here is the model's call, not a destructive resolution.
        let existing = "a".repeat(5_000);
        assert!(guard_destructive_overwrite(&existing, "tiny").is_ok());
    }

    #[test]
    fn guard_skips_tiny_conflicted_files() {
        // Very small files (< 200 bytes baseline) skip the guard so the 10 %
        // threshold doesn't fire on toy fixtures.
        let existing = "<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> base\n";
        assert!(guard_destructive_overwrite(existing, "z").is_ok());
    }

    #[test]
    fn classify_merge_output_recognizes_already_up_to_date() {
        let outcome = classify_merge_output("upstream/main".into(), "Already up to date.\n");
        assert_eq!(
            outcome,
            UpdateBranchOutcome::AlreadyUpToDate {
                base_ref: "upstream/main".into()
            }
        );
    }

    #[test]
    fn classify_merge_output_recognizes_hyphenated_already_up_to_date() {
        // Older git versions print "Already up-to-date." with hyphens.
        let outcome = classify_merge_output("origin/main".into(), "Already up-to-date.");
        assert_eq!(
            outcome,
            UpdateBranchOutcome::AlreadyUpToDate {
                base_ref: "origin/main".into()
            }
        );
    }

    #[test]
    fn classify_merge_output_detects_fast_forward() {
        let stdout = "Updating abc1234..def5678\nFast-forward\n README.md | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)\n";
        let outcome = classify_merge_output("upstream/main".into(), stdout);
        assert_eq!(
            outcome,
            UpdateBranchOutcome::FastForwarded {
                base_ref: "upstream/main".into(),
                summary: "Updating abc1234..def5678".into(),
            }
        );
    }

    #[test]
    fn classify_merge_output_treats_real_merge_as_merged() {
        let stdout = "Merge made by the 'ort' strategy.\n README.md | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)\n";
        let outcome = classify_merge_output("origin/master".into(), stdout);
        assert_eq!(
            outcome,
            UpdateBranchOutcome::Merged {
                base_ref: "origin/master".into(),
                summary: "Merge made by the 'ort' strategy.".into(),
            }
        );
    }

    #[test]
    fn parses_pr_view_json_with_title_and_body() {
        let raw = r#"{"title":"Add merge action","body":"Closes #42.\n\nNotes."}"#;
        let parsed = parse_pr_view_json(raw).unwrap();
        assert_eq!(parsed.title, "Add merge action");
        assert_eq!(parsed.body, "Closes #42.\n\nNotes.");
    }

    #[test]
    fn parses_pr_view_json_with_missing_body_as_empty() {
        let raw = r#"{"title":"Tweak copy"}"#;
        let parsed = parse_pr_view_json(raw).unwrap();
        assert_eq!(parsed.title, "Tweak copy");
        assert_eq!(parsed.body, "");
    }

    #[test]
    fn parses_pr_view_json_preserves_unicode_and_newlines() {
        // The PR body must reach `gh pr merge --body` byte-for-byte identical
        // to what GitHub stores — guard against any silent munging in the
        // serde path.
        let raw = r#"{"title":"🚀 ship it","body":"line one\nline two\n• emoji ✅"}"#;
        let parsed = parse_pr_view_json(raw).unwrap();
        assert_eq!(parsed.title, "🚀 ship it");
        assert_eq!(parsed.body, "line one\nline two\n• emoji ✅");
    }

    #[test]
    fn parse_pr_view_json_rejects_invalid_json() {
        let err = parse_pr_view_json("not json at all").unwrap_err();
        let message = format!("{err}");
        assert!(
            message.contains("invalid gh pr view output"),
            "unexpected error message: {message}"
        );
    }

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
    fn graphql_query_includes_review_fields() {
        let q = build_graphql_query("owner", "repo", &["feat"]);
        assert!(
            q.contains("reviewDecision"),
            "query must request reviewDecision so we can render the review emoji: {q}"
        );
        assert!(
            q.contains("reviewRequests(first: 100)"),
            "query must request reviewRequests with reviewer logins to detect re-requests: {q}"
        );
        assert!(q.contains("requestedReviewer"));
        assert!(
            q.contains("latestOpinionatedReviews"),
            "query must request latestOpinionatedReviews so we know who left CHANGES_REQUESTED: {q}"
        );
        assert!(
            q.contains("latestReviews"),
            "query must request latestReviews so the footer can attribute COMMENTED reviewers: {q}"
        );
        assert!(q.contains("totalCount"));
    }

    #[test]
    fn build_reviewer_summary_buckets_each_state() {
        let requested = logins(["pending_user"]);
        let nodes = vec![
            GhLatestReviewNode {
                state: Some("APPROVED".into()),
                author: Some(GhReviewAuthor {
                    login: Some("alice".into()),
                }),
            },
            GhLatestReviewNode {
                state: Some("CHANGES_REQUESTED".into()),
                author: Some(GhReviewAuthor {
                    login: Some("bob".into()),
                }),
            },
            GhLatestReviewNode {
                state: Some("COMMENTED".into()),
                author: Some(GhReviewAuthor {
                    login: Some("carol".into()),
                }),
            },
        ];
        let summary = build_reviewer_summary(&requested, &nodes);
        assert_eq!(summary.pending, vec!["pending_user".to_string()]);
        assert_eq!(summary.approved, vec!["alice".to_string()]);
        assert_eq!(summary.changes_requested, vec!["bob".to_string()]);
        assert_eq!(summary.commented, vec!["carol".to_string()]);
    }

    #[test]
    fn build_reviewer_summary_prefers_pending_over_past_review() {
        // A reviewer who left CHANGES_REQUESTED and was then re-requested
        // is back to "Pending" in the GitHub UI — the footer should match,
        // not also list them under Rejected.
        let requested = logins(["mrprey"]);
        let nodes = vec![GhLatestReviewNode {
            state: Some("CHANGES_REQUESTED".into()),
            author: Some(GhReviewAuthor {
                login: Some("mrprey".into()),
            }),
        }];
        let summary = build_reviewer_summary(&requested, &nodes);
        assert_eq!(summary.pending, vec!["mrprey".to_string()]);
        assert!(summary.changes_requested.is_empty());
    }

    #[test]
    fn parses_graphql_response_populates_reviewer_summary() {
        let body = r#"{
          "data": {
            "repository": {
              "b0": {"nodes": [{
                "number": 21,
                "state": "OPEN",
                "url": "u",
                "title": "t",
                "isDraft": false,
                "reviewDecision": "APPROVED",
                "reviewRequests": {
                  "totalCount": 1,
                  "nodes": [
                    {"requestedReviewer": {"__typename": "User", "login": "dan"}}
                  ]
                },
                "latestReviews": {
                  "nodes": [
                    {"state": "APPROVED", "author": {"login": "alice"}},
                    {"state": "CHANGES_REQUESTED", "author": {"login": "bob"}},
                    {"state": "COMMENTED", "author": {"login": "carol"}}
                  ]
                }
              }]}
            }
          }
        }"#;
        let out = parse_graphql_response(body, &["feat"]).unwrap();
        let pr = out.get("feat").unwrap().as_ref().unwrap();
        assert_eq!(pr.reviewers.pending, vec!["dan".to_string()]);
        assert_eq!(pr.reviewers.approved, vec!["alice".to_string()]);
        assert_eq!(pr.reviewers.changes_requested, vec!["bob".to_string()]);
        assert_eq!(pr.reviewers.commented, vec!["carol".to_string()]);
    }

    fn logins<I: IntoIterator<Item = &'static str>>(values: I) -> HashSet<String> {
        values.into_iter().map(String::from).collect()
    }

    #[test]
    fn derive_review_status_maps_known_decisions() {
        let empty = HashSet::new();
        assert_eq!(
            derive_review_status(Some("APPROVED"), 0, &empty, &empty),
            Some(ReviewStatus::Approved)
        );
        assert_eq!(
            derive_review_status(Some("CHANGES_REQUESTED"), 0, &logins(["alice"]), &empty),
            Some(ReviewStatus::Rejected)
        );
        assert_eq!(
            derive_review_status(Some("REVIEW_REQUIRED"), 0, &empty, &empty),
            Some(ReviewStatus::Pending)
        );
    }

    #[test]
    fn derive_review_status_flips_to_pending_when_author_re_requests_changes_reviewer() {
        // GitHub keeps reviewDecision = CHANGES_REQUESTED after the author
        // re-requests the rejecting reviewer; we detect that by seeing every
        // CHANGES_REQUESTED user listed back in reviewRequests and surface
        // Pending to mirror what the Reviewers panel shows.
        let changes = logins(["mrprey"]);
        let pending = logins(["mrprey", "tiagogoncalves"]);
        assert_eq!(
            derive_review_status(Some("CHANGES_REQUESTED"), 2, &changes, &pending),
            Some(ReviewStatus::Pending)
        );
    }

    #[test]
    fn derive_review_status_stays_rejected_when_changes_requester_not_re_requested() {
        // Some reviewers are still pending initial review, but the user who
        // rejected hasn't been re-requested — author still needs to react,
        // so we keep Rejected.
        let changes = logins(["alice"]);
        let pending = logins(["bob"]);
        assert_eq!(
            derive_review_status(Some("CHANGES_REQUESTED"), 1, &changes, &pending),
            Some(ReviewStatus::Rejected)
        );
    }

    #[test]
    fn derive_review_status_uses_pending_requests_when_decision_missing() {
        let empty = HashSet::new();
        assert_eq!(
            derive_review_status(None, 2, &empty, &empty),
            Some(ReviewStatus::Pending),
            "null decision with outstanding reviewer requests must surface as Pending"
        );
        assert_eq!(
            derive_review_status(None, 0, &empty, &empty),
            None,
            "null decision with no requests means nobody was asked yet"
        );
    }

    #[test]
    fn derive_review_status_treats_unknown_decision_as_none_unless_pending() {
        let empty = HashSet::new();
        assert_eq!(
            derive_review_status(Some("COMMENTED"), 0, &empty, &empty),
            None
        );
        assert_eq!(
            derive_review_status(Some("COMMENTED"), 1, &empty, &empty),
            Some(ReviewStatus::Pending),
            "outstanding requests still surface as pending even when decision is unrecognized"
        );
    }

    #[test]
    fn parses_graphql_response_with_review_decision() {
        let body = r#"{
          "data": {
            "repository": {
              "b0": {"nodes": [{
                "number": 11,
                "state": "OPEN",
                "url": "u",
                "title": "t",
                "isDraft": false,
                "reviewDecision": "APPROVED",
                "reviewRequests": {"totalCount": 0}
              }]}
            }
          }
        }"#;
        let out = parse_graphql_response(body, &["feat"]).unwrap();
        let pr = out.get("feat").unwrap().as_ref().unwrap();
        assert_eq!(pr.review_status, Some(ReviewStatus::Approved));
    }

    #[test]
    fn parses_graphql_response_treats_outstanding_requests_as_pending() {
        let body = r#"{
          "data": {
            "repository": {
              "b0": {"nodes": [{
                "number": 12,
                "state": "OPEN",
                "url": "u",
                "title": "t",
                "isDraft": false,
                "reviewDecision": null,
                "reviewRequests": {"totalCount": 3}
              }]}
            }
          }
        }"#;
        let out = parse_graphql_response(body, &["feat"]).unwrap();
        let pr = out.get("feat").unwrap().as_ref().unwrap();
        assert_eq!(pr.review_status, Some(ReviewStatus::Pending));
    }

    #[test]
    fn parses_graphql_response_treats_re_requested_changes_reviewer_as_pending() {
        // PR has reviewDecision = CHANGES_REQUESTED from Mrprey, but the
        // author re-requested Mrprey's review, so Mrprey is back in
        // reviewRequests. The dashboard must render "Pending" (✋), not
        // "Rejected" (👎), matching what GitHub's Reviewers panel shows.
        let body = r#"{
          "data": {
            "repository": {
              "b0": {"nodes": [{
                "number": 4288,
                "state": "OPEN",
                "url": "u",
                "title": "t",
                "isDraft": false,
                "reviewDecision": "CHANGES_REQUESTED",
                "reviewRequests": {
                  "totalCount": 2,
                  "nodes": [
                    {"requestedReviewer": {"__typename": "User", "login": "tiagogoncalves"}},
                    {"requestedReviewer": {"__typename": "User", "login": "mrprey"}}
                  ]
                },
                "latestOpinionatedReviews": {
                  "nodes": [
                    {"state": "CHANGES_REQUESTED", "author": {"login": "mrprey"}}
                  ]
                }
              }]}
            }
          }
        }"#;
        let out = parse_graphql_response(body, &["feat"]).unwrap();
        let pr = out.get("feat").unwrap().as_ref().unwrap();
        assert_eq!(pr.review_status, Some(ReviewStatus::Pending));
    }

    #[test]
    fn parses_graphql_response_keeps_rejected_when_changes_reviewer_not_re_requested() {
        // CHANGES_REQUESTED reviewer is not back in reviewRequests, so the
        // author still owes them a response — keep Rejected.
        let body = r#"{
          "data": {
            "repository": {
              "b0": {"nodes": [{
                "number": 1,
                "state": "OPEN",
                "url": "u",
                "title": "t",
                "isDraft": false,
                "reviewDecision": "CHANGES_REQUESTED",
                "reviewRequests": {
                  "totalCount": 1,
                  "nodes": [
                    {"requestedReviewer": {"__typename": "User", "login": "bob"}}
                  ]
                },
                "latestOpinionatedReviews": {
                  "nodes": [
                    {"state": "CHANGES_REQUESTED", "author": {"login": "alice"}}
                  ]
                }
              }]}
            }
          }
        }"#;
        let out = parse_graphql_response(body, &["feat"]).unwrap();
        let pr = out.get("feat").unwrap().as_ref().unwrap();
        assert_eq!(pr.review_status, Some(ReviewStatus::Rejected));
    }

    #[test]
    fn parses_graphql_response_without_review_fields_keeps_status_none() {
        let body = r#"{
          "data": {
            "repository": {
              "b0": {"nodes": [{
                "number": 13,
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
        assert_eq!(pr.review_status, None);
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

    #[test]
    fn build_merge_prompt_has_no_yaml_frontmatter() {
        // Regression guard: a leading `--- name: X ---` block would make
        // Gemini's CLI treat the prompt as a skill manifest and package it
        // into a `.skill` archive instead of resolving conflicts.
        let prompt = build_merge_prompt("upstream/main", &["a.rs".to_string()]);
        assert!(
            !prompt.trim_start().starts_with("---"),
            "prompt must not start with YAML frontmatter, got:\n{prompt}"
        );
    }

    #[test]
    fn build_merge_prompt_substitutes_base_ref_and_conflicts() {
        let prompt = build_merge_prompt(
            "upstream/main",
            &["src/foo.rs".to_string(), "tests/snap.snap".to_string()],
        );
        assert!(!prompt.contains("MERGE_REF"));
        assert!(
            prompt.contains("upstream/main"),
            "prompt should reference the resolved base ref, got:\n{prompt}"
        );
        assert!(prompt.contains("  - src/foo.rs"));
        assert!(prompt.contains("  - tests/snap.snap"));
        assert!(!prompt.contains("CONFLICTED_FILES"));
    }

    #[test]
    fn build_merge_prompt_forbids_skill_creation_and_pipeline_git_ops() {
        let prompt = build_merge_prompt("upstream/main", &["a.rs".to_string()]);
        // The prompt must tell Gemini to stay out of pipeline-managed git ops
        // and not to package itself as a skill — both are concrete failure
        // modes we've already observed in production.
        assert!(prompt.contains("git commit"));
        assert!(prompt.contains("git push"));
        assert!(prompt.to_lowercase().contains("skill"));
    }

    #[test]
    fn format_stream_event_preserves_assistant_delta_spacing() {
        let event = format_stream_event(
            r#"{"type":"message","role":"assistant","content":" hello","delta":true}"#,
        )
        .unwrap();
        assert_eq!(
            event,
            AiActivityEvent::AssistantText {
                content: " hello".to_string(),
            }
        );
    }

    #[test]
    fn format_stream_event_parses_successful_tool_results() {
        let event = format_stream_event(
            r#"{"type":"tool_result","tool_id":"run_shell_command__123","status":"success","output":"modified src/main.rs\n1 file changed"}"#,
        )
        .unwrap();
        assert_eq!(
            event,
            AiActivityEvent::ToolResult {
                tool_name: Some("run_shell_command".to_string()),
                status: AiToolResultStatus::Success,
                detail: "modified src/main.rs 1 file changed".to_string(),
            }
        );
    }

    #[test]
    fn format_stream_event_parses_structured_errors() {
        let warning = format_stream_event(
            r#"{"type":"error","severity":"warning","message":"Quota getting low"}"#,
        )
        .unwrap();
        assert_eq!(
            warning,
            AiActivityEvent::Notice {
                severity: AiActivitySeverity::Warning,
                message: "Quota getting low".to_string(),
            }
        );

        let tool_error = format_stream_event(
            r#"{"type":"tool_result","tool_id":"read_file__456","status":"error","error":{"type":"TOOL_EXECUTION_ERROR","message":"ENOENT: missing file"}}"#,
        )
        .unwrap();
        assert_eq!(
            tool_error,
            AiActivityEvent::ToolResult {
                tool_name: Some("read_file".to_string()),
                status: AiToolResultStatus::Error,
                detail: "ENOENT: missing file".to_string(),
            }
        );
    }

    #[test]
    fn format_stream_event_is_ready_for_future_thinking_events() {
        let event =
            format_stream_event(r#"{"type":"thinking","content":"Scanning both sides"}"#).unwrap();
        assert_eq!(
            event,
            AiActivityEvent::Thinking {
                content: "Scanning both sides".to_string(),
            }
        );
    }

    #[test]
    fn build_gemini_request_enables_tools_and_thinking() {
        let request = build_gemini_request(
            "gemini-3.1-pro-preview",
            "system",
            &[json!({"role": "user", "parts": [{"text": "hello"}]})],
        );

        assert_eq!(
            request["generationConfig"]["thinkingConfig"]["includeThoughts"],
            json!(true)
        );
        assert_eq!(
            request["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            json!("HIGH")
        );
        assert_eq!(
            request["tools"][0]["functionDeclarations"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn apply_gemini_sse_event_collects_thoughts_and_function_calls() {
        let event = r#"data: {"candidates":[{"content":{"parts":[{"text":"scan first","thought":true},{"functionCall":{"name":"read_file","args":{"path":"README.md"}}}]},"finishReason":"STOP"}],"usageMetadata":{"totalTokenCount":123}}"#;
        let mut turn = GeminiTurnResult::default();

        apply_gemini_sse_event(event, &mut turn, None, Instant::now()).unwrap();

        assert_eq!(turn.finish_reason.as_deref(), Some("STOP"));
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].name, "read_file");
        assert_eq!(turn.tool_calls[0].args["path"], json!("README.md"));
        assert_eq!(turn.model_parts[0]["text"], json!("scan first"));
        assert_eq!(turn.model_parts[0]["thought"], json!(true));
        assert_eq!(
            turn.model_parts[1]["functionCall"]["name"],
            json!("read_file")
        );
    }

    #[test]
    fn validate_shell_command_rejects_stateful_git_ops_and_bulk_add() {
        assert!(validate_shell_command("git status && git merge origin/main").is_err());
        assert!(validate_shell_command("git add -A").is_err());
        assert!(validate_shell_command("git add README.md && git diff --cached").is_ok());
        assert!(validate_shell_command("git merge-base HEAD origin/main").is_ok());
    }

    #[test]
    fn selected_gemini_cli_auth_type_reads_oauth_personal() {
        let settings: GeminiCliSettingsFile =
            serde_json::from_str(r#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#)
                .unwrap();
        assert_eq!(
            selected_gemini_cli_auth_type(&settings).as_deref(),
            Some("oauth-personal")
        );
    }

    #[test]
    fn subject_with_pr_reference_appends_pr_number() {
        assert_eq!(
            subject_with_pr_reference("Improve dashboard footer details", 42),
            "Improve dashboard footer details (#42)"
        );
    }

    #[test]
    fn subject_with_pr_reference_is_idempotent_when_already_present() {
        assert_eq!(
            subject_with_pr_reference("Improve dashboard footer details (#42)", 42),
            "Improve dashboard footer details (#42)"
        );
    }

    #[test]
    fn subject_with_pr_reference_trims_trailing_whitespace_before_appending() {
        assert_eq!(
            subject_with_pr_reference("Add merge action   ", 7),
            "Add merge action (#7)"
        );
    }

    #[test]
    fn subject_with_pr_reference_does_not_dedupe_different_numbers() {
        // A stale `(#99)` baked into a PR title should not stop us from
        // appending the *correct* `(#7)` reference for the PR we're merging.
        assert_eq!(
            subject_with_pr_reference("Add merge action (#99)", 7),
            "Add merge action (#99) (#7)"
        );
    }
}

//! Live dashboard polling service.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{self, MissedTickBehavior};

use crate::config::schema::{normalize_dashboard_columns, DashboardConfig};
use crate::constants::dashboard_pr_cache_file;
use crate::errors::{handle_git_error, Result, WisetreeError};
use crate::files::{strip_ansi, ActivityKind};
use crate::git::exec::execute_git_command;
use crate::git::lock::{git_lock_path, retry_on_git_lock};
use crate::git::types::{BranchStatus, GitWorktree};
use crate::services::ai_status::{AiStatusIndex, AiStatusPaths, AiStatusReport, AiStatusService};
use crate::services::bugkill::{
    attempt_commit_prefix, attempt_commit_subject, parse_investigation_md, parse_judge_verdict,
    parse_porcelain_v2, AttemptChanges, BugHypothesis, BugkillVerdict, JudgeResult,
    ParsedInvestigation, INVESTIGATION_FILE,
};
use crate::services::develop::{parse_plan_md, DevelopPlan, PLAN_FILE};

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
/// Bound on the background single-branch base fetch that keeps the base's
/// remote-tracking ref fresh for the behind-count. A scoped fetch normally
/// completes in ~1s; the cap stops a slow network from stalling the on-cycle
/// tick.
const BASE_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeouts for the "Enrich Pull Request" pipeline. Gathering the diff/log is
/// read-only but can be large on a long branch; push + `gh pr create/edit`
/// talk to the network.
const ENRICH_CONTEXT_TIMEOUT: Duration = Duration::from_secs(30);
const ENRICH_PUSH_TIMEOUT: Duration = Duration::from_secs(60);
const ENRICH_SUBMIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Soft cap on the embedded diff size inside the AI prompt. opencode
/// receives the whole prompt as a single argv entry, so an unbounded diff
/// risks exceeding the OS argument-length limit and failing to spawn.
/// Diffs above this are truncated with a marker so the launch always
/// succeeds; the AI still has the commit log + the bulk of the diff to work
/// from.
const ENRICH_DIFF_MAX_BYTES: usize = 120_000;
/// Timeouts for the "Fix Pull Request" pipeline (resolve review comments).
/// Sync + fetch are network paths; the captured planning call drives a full
/// model turn so it gets the longest leash; commit/reply/push are local-ish.
const FIX_SYNC_TIMEOUT: Duration = Duration::from_secs(60);
const FIX_FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const FIX_PLAN_TIMEOUT: Duration = Duration::from_secs(180);
const FIX_COMMIT_TIMEOUT: Duration = Duration::from_secs(30);
const FIX_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const FIX_PUSH_TIMEOUT: Duration = Duration::from_secs(60);
/// How many lines of context to show the planning AI on each side of a
/// commented line, and a hard byte cap so the captured prompt never blows
/// past the OS argv limit.
const FIX_CODE_WINDOW_RADIUS: usize = 40;
const FIX_CODE_MAX_BYTES: usize = 24_000;
/// Timeouts for the "Review Pull Request" pipeline (scan the diff, post
/// comments). Sync + fetch are network paths; each captured per-file scan
/// drives a full model turn so it gets the longest leash; posting a comment
/// and submitting the review summary are single API calls.
const REVIEW_SYNC_TIMEOUT: Duration = Duration::from_secs(60);
const REVIEW_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const REVIEW_SCAN_TIMEOUT: Duration = Duration::from_secs(240);
const REVIEW_POST_TIMEOUT: Duration = Duration::from_secs(30);
/// GitHub's API occasionally answers a healthy PR with a transient 5xx (a
/// blip in their infra, not a real problem with the PR or its diff). Retried
/// a couple of times in [`run_gh_command`] so a single blip during Review/Fix
/// prep doesn't force the user back to the Dashboard.
const GH_TRANSIENT_RETRIES: u32 = 2;
const GH_TRANSIENT_RETRY_DELAY: Duration = Duration::from_secs(3);
/// Byte caps applied before templating the per-file scan prompt, so the
/// rendered prompt argv never approaches OS limits: one file's annotated
/// diff, and the existing-comments dedup context.
const REVIEW_DIFF_MAX_BYTES: usize = 60_000;
const REVIEW_COMMENTS_MAX_BYTES: usize = 12_000;
/// Byte cap for the whole-diff coverage prompt, which concatenates every
/// file's (already individually capped) annotated diff into one call.
const REVIEW_COVERAGE_DIFF_MAX_BYTES: usize = 120_000;
/// Timeouts for the "Bugkill" pipeline. The investigation runs live in the
/// embedded PTY (the user watches it and can Esc out), so only the judge —
/// which classifies one short comment — and the local git operations
/// (status/add/commit/revert/checkout) run captured under a timeout.
const BUGKILL_JUDGE_TIMEOUT: Duration = Duration::from_secs(120);
const BUGKILL_GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Byte caps applied to Bugkill prompt inputs before templating so the
/// prompt argv never approaches OS limits.
const BUGKILL_DESCRIPTION_MAX_BYTES: usize = 16_000;
const BUGKILL_FIELD_MAX_BYTES: usize = 8_000;
/// Byte caps applied to Develop prompt inputs before templating. The
/// previous-plan block embedded on a revision can hold a whole plan, so it
/// gets a roomier cap than the freeform fields.
const DEVELOP_TASK_MAX_BYTES: usize = 16_000;
const DEVELOP_FEEDBACK_MAX_BYTES: usize = 8_000;
const DEVELOP_PLAN_MAX_BYTES: usize = 48_000;
/// How long the post-section check command may run before it is killed and
/// reported as a failure. A full test suite is slower than a git op, so this
/// gets a generous leash.
const DEVELOP_CHECK_TIMEOUT: Duration = Duration::from_secs(600);
/// Bytes of the check command's combined output kept (the tail) for the UI
/// and the corrective prompt — the failing assertions are near the end.
const DEVELOP_CHECK_OUTPUT_MAX_BYTES: usize = 12_000;
/// The harness-owned plan file, always excluded from a section commit — like
/// Bugkill's `BUG_INVESTIGATION.md`, it is output for the human, not part of
/// the delivered change.
const DEVELOP_PLAN_FILE: &str = "PLAN.md";
/// Priority list for the base ref the "Update Pull Request" flow merges
/// in. Kept in one place so the dashboard's behind probe and the update
/// pipeline never drift apart.
pub const BASE_REF_PRIORITY: [&str; 6] = [
    "upstream/main",
    "upstream/master",
    "upstream/develop",
    "origin/main",
    "origin/master",
    "origin/develop",
];
/// How often the service refetches PR data when branches are otherwise idle.
/// Catches remote-only changes (merge, close, title edit) without hammering
/// the API. The Status column countdown is driven by the same timer.
pub const PR_REFRESH_PERIOD_MS: u64 = 30 * 1000;
const WISE_MERGE_FAILURE_BACKOFF: Duration = Duration::from_secs(60);
/// Per-tick budget for the global AI-status scan. When exceeded, the index
/// degrades to empty for this tick and every worktree renders `⬜ Pending`
/// rather than blocking the whole dashboard refresh.
pub const AI_STATUS_BUDGET_MS: u64 = 200;
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

/// Structured event emitted by the streamed Opencode subprocess adapter.
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
    ConflictsDetected { count: usize },
    AiResolving { model: String },
    Committing,
    Pushing,
}

/// Outcome of the `update_pull_request` pipeline. Surfaced verbatim to the
/// UI which maps each variant to a palette-colored toast or to a follow-up
/// state (in the case of `AiResolutionComplete`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdatePullRequestOutcome {
    /// The recheck after `git fetch` showed the branch was already up to
    /// date with the resolved base ref. No merge or push was attempted.
    AlreadyUpToDate,
    /// `git merge` succeeded with no conflicts; the result has been pushed.
    MergedCleanly,
    /// A push-only run (`Push Pull Request` action, or the Terminal recovery
    /// re-push) sent `git push origin HEAD` and it succeeded. No merge was
    /// attempted — the branch was already ahead-but-not-behind.
    Pushed,
    /// Conflicts were detected, `ai.model` is set, opencode is on PATH, and
    /// the merge is paused mid-flight (index has conflict markers). The
    /// UI takes over from here: it spawns opencode inside an embedded
    /// PTY so the user sees the real opencode TUI in the AI Activity
    /// panel. Once opencode exits, the screen surfaces Complete/Cancel
    /// which dispatches `commit_and_push_ai_merge` or `abort_ai_merge`.
    ConflictsHandedOffToUi {
        opencode_binary: PathBuf,
        opencode_args: Vec<String>,
        cwd: PathBuf,
        model: String,
        base_ref: String,
        conflicts: Vec<String>,
    },
    /// Conflicts were detected but `ai.model` is blank in DashboardConfig, so
    /// no AI is available to resolve them. The merge has been aborted and
    /// the worktree is clean again. The list of conflicted files is
    /// included so the toast can show how many files need attention.
    ConflictsRequireAi { conflicts: Vec<String> },
    /// Conflicts were detected, `ai.model` is set, but the `opencode` binary
    /// is not on PATH. The merge has been aborted; the worktree is clean
    /// again.
    AiUnavailable { conflicts: Vec<String> },
    /// Commit + push after AI resolution succeeded. Returned by
    /// `commit_and_push_ai_merge`.
    MergedWithAiResolution,
    /// User cancelled the AI merge and `git merge --abort` succeeded.
    /// Returned by `abort_ai_merge`.
    DiscardedAiMerge,
    /// `git merge --abort` failed during the cancel flow. stderr included.
    AbortFailed(String),
    /// `git fetch` failed (network, auth, …). stderr included.
    FetchFailed(String),
    /// `git merge` failed for a non-conflict reason (e.g. dirty tree), or
    /// opencode ran but conflicts remained, or `git add`/`git commit`
    /// failed during AI resolution. stderr/details included.
    MergeFailed(String),
    /// Both `git push upstream HEAD` and `git push origin HEAD` failed.
    /// stderr included.
    PushFailed(String),
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
    /// `git merge {base_ref}` failed for a *non-conflict* reason (dirty
    /// tree, refusal, ...). stderr included. Genuine merge conflicts are
    /// reported via the `Conflicts*` variants below instead.
    MergeFailed { base_ref: String, message: String },
    /// The worktree has uncommitted tracked changes, so `git merge` would
    /// refuse to start ("Your local changes ... would be overwritten by
    /// merge"). Caught as a pre-flight guard *before* fetch/merge. This is
    /// not a merge conflict — there are no markers and nothing for opencode
    /// to resolve — so the UI just tells the user to commit or stash first.
    WorkingTreeDirty { files: Vec<String> },
    /// Merge conflicts were detected, `ai.model` is set, and `opencode` is on
    /// PATH. The merge is left mid-flight (conflict markers in the index)
    /// and the UI takes over: it spawns opencode inside an embedded PTY to
    /// resolve the conflicts, then commits the result locally (no push).
    /// Mirrors `UpdatePullRequestOutcome::ConflictsHandedOffToUi`.
    ConflictsHandedOffToUi {
        opencode_binary: PathBuf,
        opencode_args: Vec<String>,
        cwd: PathBuf,
        model: String,
        base_ref: String,
        conflicts: Vec<String>,
    },
    /// Merge conflicts were detected but `ai.model` is blank — the merge is
    /// aborted and the UI prompts the user to configure `ai.model` or resolve
    /// the conflicts manually.
    ConflictsRequireAi { conflicts: Vec<String> },
    /// Merge conflicts were detected and `ai.model` is set, but the `opencode`
    /// binary is not on PATH — the merge is aborted and the UI prompts the
    /// user to install opencode.
    AiUnavailable { conflicts: Vec<String> },
}

/// Result of the read-only preparation phase of the "Enrich Pull Request"
/// pipeline (`prepare_enrich`). On `HandedOffToUi` the UI spawns opencode in
/// its embedded PTY to draft `pull_request.md`; the other variants are
/// terminal and map straight to a toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrichPreparation {
    /// Diff/log gathered, prompt built, `ai.model` set and `opencode` on PATH.
    /// The UI owns the PTY lifecycle from here; once opencode finishes the
    /// screen reads `pull_request.md` and offers to open/update the PR.
    HandedOffToUi {
        opencode_binary: PathBuf,
        opencode_args: Vec<String>,
        cwd: PathBuf,
        model: String,
    },
    /// No commits ahead of the base ref → there is nothing to describe.
    NothingToDescribe,
    /// `ai.model` is blank in `DashboardConfig` — no model configured to draft.
    AiNotConfigured,
    /// `ai.model` is set but the `opencode` binary is not on PATH.
    AiUnavailable,
}

/// Parameters for opening or updating a pull request via `submit_pull_request`.
pub struct EnrichSubmitRequest {
    pub worktree_path: String,
    pub branch: String,
    /// `Some` → update an existing PR; `None` → push + create a new one.
    pub number: Option<u64>,
    /// The resolved base ref (`upstream/release-0.41`, `origin/main`, …) the
    /// new PR should target. Passed to `gh pr create --base <branch>` (the
    /// remote prefix is stripped) so a worktree cut from a non-default branch
    /// opens its PR against that branch instead of the repo default. `None`
    /// when unresolved, or on the update path where the base is fixed.
    pub base_ref: Option<String>,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    /// Title the PR already has on GitHub (update path only). When non-empty,
    /// `--title` is omitted from `gh pr edit` so the existing title is preserved.
    pub existing_title: Option<String>,
    /// Labels the PR already has on GitHub (update path only). When non-empty,
    /// `--add-label` is omitted from `gh pr edit` so existing labels are preserved.
    pub existing_labels: Vec<String>,
}

/// Outcome of submitting the drafted PR (`submit_pull_request`): either a
/// brand-new PR was created or the existing one's title/body were updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrichSubmitOutcome {
    /// `gh pr create` succeeded. `url` is parsed from gh's stdout.
    Created { number: u64, url: String },
    /// `gh pr edit` succeeded for the existing PR.
    Updated { number: u64 },
    /// `git push` failed before the PR could be created. stderr included.
    PushFailed(String),
    /// `gh pr create` / `gh pr edit` failed. stderr included.
    SubmitFailed(String),
}

/// One reviewer comment retained after filtering. `body` is the raw markdown
/// the reviewer wrote; `author` is their GitHub login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewComment {
    pub author: String,
    pub body: String,
    /// `databaseId` of this inline comment, used to anchor a reaction to the
    /// exact comment. `None` for PR-level review summaries (no comment anchor).
    pub database_id: Option<u64>,
    /// True when the viewer (the PR author running Fix) wrote this comment.
    /// Lets us skip our own replies when locating the reviewer's praise.
    pub viewer_did_author: bool,
}

/// A group of inline review comments that target the same file + line. The
/// whole group is judged by one planning call and resolved as a single unit
/// (Apply / Other / Skip). `file` / `line` may still be `None` for a comment
/// GitHub no longer maps to a current line; the reply then anchors via
/// `reply_comment_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentGroup {
    /// File the comments target; `None` when GitHub returns no path.
    pub file: Option<String>,
    /// Line the comments target; `None` when not line-anchored.
    pub line: Option<u64>,
    /// `databaseId` of the inline comment to reply to. `None` only when the
    /// comment lacks an id, in which case the reply falls back to a PR comment.
    pub reply_comment_id: Option<u64>,
    pub comments: Vec<ReviewComment>,
}

impl CommentGroup {
    /// Short human label for toasts and the summary table: `path:line`, or a
    /// fallback when GitHub returns no path for the comment.
    pub fn descriptor(&self) -> String {
        match (&self.file, self.line) {
            (Some(file), Some(line)) => format!("{file}:{line}"),
            (Some(file), None) => file.clone(),
            _ => "PR review comment".to_string(),
        }
    }

    /// The comment text fed to the planning prompt: each comment prefixed
    /// with its author so the AI can weigh who said what.
    pub fn combined_text(&self) -> String {
        self.comments
            .iter()
            .map(|c| format!("@{}: {}", c.author, c.body.trim()))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// First comment's body, collapsed to a single line and clipped — quoted
    /// inside the commit body as the reviewer's feedback.
    fn brief(&self) -> String {
        let raw = self.comments.first().map(|c| c.body.as_str()).unwrap_or("");
        let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        clip(&one_line, 80)
    }

    /// The inline comment to react to when the thread ends in praise: the
    /// reviewer's *most recent* comment — the one actually praising — not the
    /// first comment, which is usually the original change request. Threads
    /// often run "reviewer asks for a change → we justify → reviewer concedes
    /// and praises"; the 😄 belongs on that closing remark. Returns `None`
    /// (no reaction) when no reviewer comment carries an id, e.g. PR-level
    /// summaries, where reacting via the inline-comment endpoint is impossible.
    pub fn praise_reaction_target_id(&self) -> Option<u64> {
        self.comments
            .iter()
            .rev()
            .find(|c| !c.viewer_did_author && c.database_id.is_some())
            .and_then(|c| c.database_id)
    }
}

/// Outcome of the read-only Fix preparation (`prepare_fix`): sync the branch,
/// then fetch + filter + group the PR's review comments. `Ready` hands the
/// groups to the UI which drives the per-comment plan/apply loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixPreparation {
    Ready {
        groups: Vec<CommentGroup>,
        owner: String,
        repo: String,
    },
    /// No unresolved review comments remain after filtering.
    NoComments,
    /// `gh` CLI is missing.
    GhUnavailable,
    /// `ai.model` is blank — no model configured to plan fixes.
    AiNotConfigured,
    /// `ai.model` set but `opencode` is not on PATH.
    AiUnavailable,
    /// `git pull --ff-only` failed (divergence / network). stderr included.
    SyncFailed(String),
}

/// The structured fields the planning AI emits for a `fix` verdict. Rendered
/// to the user on the Decision step and fed into the apply prompt + commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixPlan {
    /// One short imperative line for the commit subject.
    pub summary: String,
    pub validity: String,
    pub explanation: String,
    pub change: String,
}

/// The verdict the planning AI returns for one comment group. The harness
/// branches on this deterministically — never asks the AI to route itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixVerdict {
    /// Pure acknowledgement — skip silently.
    Praise,
    /// Non-actionable — post this reply, no code change.
    Reply(String),
    /// Actionable — present the plan with Apply / Other / Skip.
    Fix(FixPlan),
}

/// Spawn parameters for the live apply phase, mirroring
/// [`EnrichPreparation::HandedOffToUi`]. The UI owns the PTY from here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixApplyHandoff {
    pub opencode_binary: PathBuf,
    pub opencode_args: Vec<String>,
    pub cwd: PathBuf,
}

/// Result of the post-apply [`DashboardService::commit_and_reply`] step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixCommitOutcome {
    /// opencode edited the file(s): the change was committed and the reviewer
    /// was replied to with the commit link.
    Committed,
    /// opencode made no change — on closer inspection the code already
    /// satisfied the comment. No commit was created; the reviewer was told it
    /// is already addressed. Not a failure.
    AlreadyResolved,
}

/// One changed file extracted from the PR diff by the Review pipeline. The
/// annotated diff (new-side line numbers inlined) and the existing comments
/// are everything one per-file scan call needs; `commentable_lines` is the
/// deterministic ground truth the AI's line anchors are validated against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFile {
    pub path: String,
    /// This file's hunks with every new-side line prefixed by its line
    /// number, so the AI cites anchors the harness can verify.
    pub annotated_diff: String,
    /// New-side line numbers GitHub accepts inline comments on (additions +
    /// context lines present in the diff).
    pub commentable_lines: BTreeSet<u64>,
    /// Review comments already posted on this file, rendered as
    /// `@author (line N): body` blocks — dedup context for the scan call.
    /// Empty when the PR has none.
    pub existing_comments: String,
    /// Structured dedup keys of the wisetree-format comments already on
    /// this file. Unlike `existing_comments` (advice the model may ignore),
    /// these back the deterministic duplicate filter in Rust.
    pub existing_keys: Vec<ExistingFindingKey>,
}

/// Dedup key of one wisetree-format comment already posted on the PR: its
/// anchor line and its normalized (lowercased) finding title. Human-written
/// comments never produce a key, so they never suppress a finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingFindingKey {
    pub line: Option<u64>,
    pub title: String,
}

/// Outcome of the read-only Review preparation (`prepare_review`): sync the
/// branch, resolve the PR, fetch its diff + existing comments, and split the
/// diff per changed file. `Ready` hands the files to the UI which drives the
/// per-file scan loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewPreparation {
    Ready {
        files: Vec<ReviewFile>,
        /// Changed files nobody reviews by hand (lockfiles, minified
        /// bundles, snapshots, …), filtered out deterministically before
        /// any AI call. Reported on the final table with their reason.
        skipped: Vec<ReviewSkippedFile>,
        owner: String,
        repo: String,
        /// PR head commit sha — required by the inline-comment API.
        head_sha: String,
    },
    /// The PR diff contains no reviewable text changes.
    NoChanges,
    /// `gh` CLI is missing.
    GhUnavailable,
    /// `ai.review.model` is blank — no model configured to scan the diff.
    AiNotConfigured,
    /// `ai.review.model` set but `opencode` is not on PATH.
    AiUnavailable,
    /// `git pull --ff-only` or the PR lookup failed. stderr included.
    SyncFailed(String),
}

/// One changed file excluded from the review before any AI call, with the
/// human-readable reason shown on the final report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSkippedFile {
    pub path: String,
    pub reason: &'static str,
}

/// Severity the scan AI assigns to a finding. Ordered so the walkthrough can
/// sort Critical → Low.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewSeverity {
    Critical,
    High,
    Medium,
    Low,
}

impl ReviewSeverity {
    pub fn label(self) -> &'static str {
        match self {
            ReviewSeverity::Critical => "Critical",
            ReviewSeverity::High => "High",
            ReviewSeverity::Medium => "Medium",
            ReviewSeverity::Low => "Low",
        }
    }

    /// Colored circle that fronts the severity in the comment footer badge.
    pub fn emoji(self) -> &'static str {
        match self {
            ReviewSeverity::Critical => "🔴",
            ReviewSeverity::High => "🟠",
            ReviewSeverity::Medium => "🟡",
            ReviewSeverity::Low => "⚪",
        }
    }

    /// Sort key: Critical first.
    pub fn rank(self) -> u8 {
        match self {
            ReviewSeverity::Critical => 0,
            ReviewSeverity::High => 1,
            ReviewSeverity::Medium => 2,
            ReviewSeverity::Low => 3,
        }
    }

    /// Tolerant parse of the AI's `SEVERITY:` value. Anything unrecognized
    /// lands on `Medium` — never inflate an unknown label to Critical.
    fn parse(value: &str) -> Self {
        match value.trim().to_lowercase().as_str() {
            "critical" => ReviewSeverity::Critical,
            "high" => ReviewSeverity::High,
            "low" => ReviewSeverity::Low,
            _ => ReviewSeverity::Medium,
        }
    }
}

/// One issue the scan AI found in a changed file. `line` / `start_line` are
/// already validated against the file's commentable lines — `line == None`
/// means the finding is posted as a general PR comment instead of inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFinding {
    /// One of the five review categories (`Code Smell`, `Security`, …).
    pub category: String,
    pub severity: ReviewSeverity,
    pub file: String,
    /// First line of a multi-line range; only ever `Some` when `line` is.
    pub start_line: Option<u64>,
    /// New-side line the inline comment anchors to; `None` → file-level.
    pub line: Option<u64>,
    pub title: String,
    pub explanation: String,
    /// Exact replacement code for the targeted line(s), when the fix is
    /// expressible as one — becomes a GitHub ```suggestion block.
    pub suggestion: Option<String>,
}

impl ReviewFinding {
    /// Short human label for the walkthrough header and the summary table:
    /// `path:line` (or `path:start-line` for a range), or just the path.
    pub fn descriptor(&self) -> String {
        match (self.start_line, self.line) {
            (Some(start), Some(line)) => format!("{}:{start}-{line}", self.file),
            (None, Some(line)) => format!("{}:{line}", self.file),
            _ => self.file.clone(),
        }
    }

    /// The exact markdown body posted to GitHub. Inline comments carry a
    /// one-click ```suggestion block; file-level comments name the file and
    /// downgrade the suggestion to a plain code block (suggestion blocks only
    /// work anchored to diff lines).
    pub fn comment_body(&self) -> String {
        let mut body = format!("### {}", self.title);
        if self.line.is_none() {
            body.push_str(&format!("\n\n📄 `{}`", self.file));
        }
        if !self.explanation.trim().is_empty() {
            body.push_str(&format!("\n\n{}", self.explanation.trim()));
        }
        if let Some(suggestion) = &self.suggestion {
            if self.line.is_some() {
                body.push_str(&format!(
                    "\n\n**Suggested fix:**\n```suggestion\n{suggestion}\n```"
                ));
            } else {
                body.push_str(&format!("\n\n**Proposed code:**\n```\n{suggestion}\n```"));
            }
        }
        body.push_str(&format!(
            "\n\n<p align=\"center\">\n{} [{} {}] [{}]\n</p>",
            self.severity.emoji(),
            category_emoji(&self.category),
            self.category,
            self.severity.label()
        ));
        body
    }

    /// The finding rendered back to text for a revision call, so the model
    /// revises rather than starts fresh.
    pub fn rendered_for_revision(&self) -> String {
        let anchor = match (self.start_line, self.line) {
            (Some(start), Some(line)) => format!("lines {start}-{line}"),
            (None, Some(line)) => format!("line {line}"),
            _ => "file-level (no line anchor)".to_string(),
        };
        let mut text = format!(
            "Category: {}\nSeverity: {}\nAnchor: {anchor}\nTitle: {}\nExplanation: {}",
            self.category,
            self.severity.label(),
            self.title,
            self.explanation
        );
        if let Some(suggestion) = &self.suggestion {
            text.push_str(&format!("\nSuggestion:\n{suggestion}"));
        }
        text
    }
}

/// Snapshot of a Bugkill worktree: tracked-change paths plus the untracked
/// files with a content hash each. Taken before an attempt as the baseline
/// and after it to compute the attempt change-set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BugkillSnapshot {
    pub tracked: Vec<String>,
    /// `(path, content hash)` for every untracked file, excluding
    /// `BUG_INVESTIGATION.md`.
    pub untracked: Vec<(String, String)>,
}

/// An attempt row recovered from `BUG_INVESTIGATION.md` that was committed
/// but never got its Verdict answer (wisetree crashed in between). Resume
/// re-enters the Verdict step for this row instead of stranding it
/// permanently ineligible. `sha` is recovered from `git log` by the
/// `bugkill: attempt #N — ` subject prefix; `None` when no such commit
/// exists (the No path then records the failure without reverting).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BugkillUnverdicted {
    pub row_number: usize,
    pub sha: Option<String>,
}

/// What the preflight found on disk about a previous Bugkill run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BugkillResumeState {
    /// No `BUG_INVESTIGATION.md` — continue straight to the investigation.
    Absent,
    /// The file exists but is not in Bugkill's format — Overwrite/Cancel.
    Unparseable,
    /// The file parses — offer Resume / Start fresh.
    Parsed {
        investigation: ParsedInvestigation,
        unverdicted: Option<BugkillUnverdicted>,
    },
}

/// Everything the deterministic pre-flight gathered for a Bugkill run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BugkillPreflight {
    /// Baseline `(path, hash)` of pre-existing untracked files — later
    /// distinguishes attempt-created files from the user's own.
    pub untracked_snapshot: Vec<(String, String)>,
    /// First reachable ref in `BASE_REF_PRIORITY`; `None` is not fatal (it
    /// is only prompt context, rendered as `(none resolved)`).
    pub base_ref: Option<String>,
    pub resume: BugkillResumeState,
}

/// Outcome of [`DashboardService::bugkill_preflight`]. The non-`Ready`
/// variants map straight to a toast or prompt in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BugkillPreflightOutcome {
    Ready(Box<BugkillPreflight>),
    /// `ai.bugkill.investigate.model` is blank.
    AiNotConfigured,
    /// `opencode` is not on PATH.
    AiUnavailable,
    /// Tracked changes and no parseable investigation file — the user must
    /// commit or stash before running Bugkill.
    DirtyTree {
        count: usize,
    },
    /// Tracked changes *plus* a parseable `BUG_INVESTIGATION.md` — almost
    /// certainly debris from a fix attempt interrupted mid-`Fixing`. The UI
    /// asks before discarding; never discard automatically.
    LeftoverAttempt {
        tracked: Vec<String>,
    },
}

/// What the Develop preflight found on disk about a previous run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevelopResumeState {
    /// No `PLAN.md` — collect a task description.
    Absent,
    /// The file exists but is not in Develop's format — Overwrite/Cancel.
    Unparseable,
    /// A parseable plan. With pending sections the UI offers Resume; a fully
    /// implemented plan only offers Start fresh.
    Parsed(DevelopPlan),
}

/// Everything the deterministic Develop pre-flight gathered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevelopPreflight {
    /// First reachable ref from `BASE_REF_PRIORITY` (prompt context only,
    /// rendered as `(none resolved)` when absent).
    pub base_ref: Option<String>,
    pub resume: DevelopResumeState,
}

/// Outcome of [`DashboardService::develop_preflight`]. The non-`Ready`
/// variants map straight to a toast in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevelopPreflightOutcome {
    Ready(Box<DevelopPreflight>),
    /// `ai.develop.plan.model` is blank.
    AiNotConfigured,
    /// `opencode` is not on PATH.
    AiUnavailable,
}

/// Result of running the configured post-section check command (Ralph-canon
/// backpressure). `Failed` carries the captured output tail so the UI can
/// show it and the corrective run can embed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevelopCheckOutcome {
    Passed,
    Failed { output: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardRow {
    #[serde(flatten)]
    pub worktree: GitWorktree,
    #[serde(rename = "lastCommit", skip_serializing_if = "Option::is_none")]
    pub last_commit: Option<CommitSummary>,
    #[serde(rename = "pullRequest", skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequest>,
    /// AI harness activity for this worktree. `None` when ai_status detection
    /// hasn't run yet (e.g. the first git-only emission, or the per-tick
    /// scan exhausted its budget).
    #[serde(rename = "aiStatus", default, skip_serializing_if = "Option::is_none")]
    pub ai_status: Option<AiStatusReport>,
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
    fn success(message: impl Into<String>) -> Self {
        Self {
            level: DashboardNoticeLevel::Success,
            message: message.into(),
        }
    }

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

#[derive(Debug, Clone)]
struct WiseMergeCandidate {
    number: u64,
    worktree_path: String,
    base_ref_name: String,
    base_repository: String,
    head_ref_oid: String,
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
    wise_merge_tasks: Option<Arc<Mutex<Vec<JoinHandle<()>>>>>,
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
        if let Some(tasks) = self.wise_merge_tasks.take() {
            let mut tasks = tasks.lock().expect("wise_merge_tasks poisoned");
            for task in tasks.drain(..) {
                task.abort();
            }
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
    /// Path to the `opencode` binary. Production points at the resolved
    /// name on PATH; tests can swap in a deterministic local stub.
    opencode_binary: PathBuf,
    cache_path: Option<PathBuf>,
    pr_state: Arc<Mutex<PrCacheState>>,
    ai_status: AiStatusService,
    /// Last successful AI-status index. When a per-tick scan exceeds
    /// `AI_STATUS_BUDGET_MS` or panics, we fall back to this instead of an
    /// empty index so rows keep their previous values instead of flickering
    /// to `⬜ Pending` and back on the next successful tick.
    last_ai_index: Arc<Mutex<Option<AiStatusIndex>>>,
    wise_merge_in_flight: Arc<Mutex<HashSet<u64>>>,
    wise_merge_merged: Arc<Mutex<HashSet<u64>>>,
    wise_merge_failed_until: Arc<Mutex<HashMap<u64, Instant>>>,
    wise_merge_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl DashboardService {
    pub fn new(git_root: PathBuf, mut config: DashboardConfig) -> Self {
        config.clamp();
        let git_binary = PathBuf::from("git");
        let gh_binary = PathBuf::from("gh");
        let gh_available = binary_available(&gh_binary);
        let ai_status = AiStatusService::new(&config.ai_status, AiStatusPaths::detect());
        Self {
            git_root,
            config,
            gh_available,
            git_binary,
            gh_binary,
            opencode_binary: PathBuf::from(crate::constants::OPENCODE_CLI_BINARY),
            cache_path: Some(dashboard_pr_cache_file()),
            pr_state: Arc::new(Mutex::new(PrCacheState::default())),
            ai_status,
            last_ai_index: Arc::new(Mutex::new(None)),
            wise_merge_in_flight: Arc::new(Mutex::new(HashSet::new())),
            wise_merge_merged: Arc::new(Mutex::new(HashSet::new())),
            wise_merge_failed_until: Arc::new(Mutex::new(HashMap::new())),
            wise_merge_tasks: Arc::new(Mutex::new(Vec::new())),
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

    /// Override the `AiStatusService` for hermetic tests so we can point
    /// detection at a `TempDir` instead of the developer's real `$HOME`.
    pub fn with_ai_status(mut self, ai_status: AiStatusService) -> Self {
        self.ai_status = ai_status;
        self
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

    /// Override the `opencode` binary with a deterministic local stub. Used
    /// by tests to drive the AI conflict resolution path without invoking
    /// the real CLI.
    pub fn with_opencode_binary(mut self, opencode_binary: PathBuf) -> Self {
        self.opencode_binary = opencode_binary;
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
        let wise_merge_tasks = service.wise_merge_tasks.clone();
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
                // Emit git-only rows (with cached PRs applied) first so the
                // UI exits "Loading dashboard..." instantly, without waiting on
                // any network round-trip. PR enrichment and the base-ref fetch
                // below are refinements that fill in afterwards.
                match service.collect_git_rows().await {
                    Ok(mut rows) => {
                        if rows_tx
                            .send(DashboardUpdate::GitOnly(rows.clone()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if enrich {
                            let fresh_prs = service.refresh_pull_requests(&rows, on_cycle).await;
                            if on_cycle {
                                next_pr_fetch_at = Some(Instant::now() + period);
                            }
                            service.apply_cached_prs(&mut rows);
                            if fresh_prs {
                                service.start_wise_merge_candidates(&rows);
                            }
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

                // On the 30s PR beat, refresh the base branch's remote-tracking
                // ref so the behind-count reflects commits another developer
                // pushed to the base — the signal the "Update" command is gated
                // on. Runs *after* the paint above so it never blocks the first
                // render; when it actually advances the ref, loop again right
                // away to re-render the refined behind-count. Best-effort: a
                // failed or no-op fetch just falls through to the normal wait.
                if on_cycle && service.fetch_base_ref().await {
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
            wise_merge_tasks: Some(wise_merge_tasks),
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
    /// screen always shows the description GitHub currently has.
    pub async fn fetch_pr_details(&self, number: u64) -> Result<PullRequestDetails> {
        self.fetch_pr_details_with_repo(number, None).await
    }

    async fn fetch_pr_details_for_repo(
        &self,
        number: u64,
        repo_slug: &str,
    ) -> Result<PullRequestDetails> {
        self.fetch_pr_details_with_repo(number, Some(repo_slug))
            .await
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
    pub async fn merge_pull_request(&self, number: u64, subject: &str, body: &str) -> Result<()> {
        self.merge_pull_request_with_options(number, subject, body, None, None)
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

    /// Push the worktree's `HEAD` to `origin` (`git push origin HEAD`). Used
    /// by the Merge flow to flush local commits into the PR before it is
    /// squash-merged, so nothing is lost.
    pub async fn push_head_to_origin(&self, worktree_path: &str) -> Result<()> {
        let cwd = PathBuf::from(worktree_path);
        with_timeout(
            "git push",
            UPDATE_PUSH_TIMEOUT,
            run_command(&self.git_binary, &["push", "origin", "HEAD"], Some(&cwd)),
        )
        .await?
        .map_err(WisetreeError::other)?;
        Ok(())
    }

    async fn merge_pull_request_in_repo(
        &self,
        number: u64,
        subject: &str,
        body: &str,
        repo_slug: &str,
        match_head_commit: &str,
    ) -> Result<()> {
        self.merge_pull_request_with_options(
            number,
            subject,
            body,
            Some(repo_slug),
            Some(match_head_commit),
        )
        .await
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

    fn start_wise_merge_candidates(&self, rows: &[DashboardRow]) {
        if !self.config.wise_merge || !self.pr_enrichment_enabled() {
            return;
        }

        for candidate in rows.iter().filter_map(wise_merge_candidate) {
            if !self.mark_wise_merge_started(candidate.number) {
                continue;
            }
            let service = self.clone();
            let task = tokio::spawn(async move {
                let number = candidate.number;
                let result = service.wise_merge(candidate).await;
                service.finish_wise_merge(number, result);
            });
            let mut tasks = self
                .wise_merge_tasks
                .lock()
                .expect("wise_merge_tasks poisoned");
            tasks.retain(|task| !task.is_finished());
            tasks.push(task);
        }
    }

    fn mark_wise_merge_started(&self, number: u64) -> bool {
        if self
            .wise_merge_merged
            .lock()
            .expect("wise_merge_merged poisoned")
            .contains(&number)
        {
            return false;
        }

        let now = Instant::now();
        {
            let mut failed_until = self
                .wise_merge_failed_until
                .lock()
                .expect("wise_merge_failed_until poisoned");
            if failed_until
                .get(&number)
                .is_some_and(|deadline| *deadline > now)
            {
                return false;
            }
            failed_until.remove(&number);
        }

        self.wise_merge_in_flight
            .lock()
            .expect("wise_merge_in_flight poisoned")
            .insert(number)
    }

    async fn wise_merge(&self, candidate: WiseMergeCandidate) -> Result<String> {
        let cwd = PathBuf::from(&candidate.worktree_path);
        let base_ref =
            resolve_base_ref_with_binary(&self.git_binary, &cwd, Some(&candidate.base_ref_name))
                .await
                .ok_or_else(|| {
                WisetreeError::other(
                    "No base ref reachable (looked for upstream/main, upstream/master, upstream/develop, origin/main, origin/master, origin/develop).",
                )
            })?;
        let base_remote = remote_name_from_ref(&base_ref).ok_or_else(|| {
            WisetreeError::other(format!(
                "Resolved base ref `{base_ref}` does not name a remote."
            ))
        })?;
        let base_repo = self.resolve_github_slug_for_remote(base_remote).await?;
        validate_wise_merge_base(&candidate, &base_ref)?;
        validate_wise_merge_repository(&candidate, &base_repo)?;
        let details = self
            .fetch_pr_details_for_repo(candidate.number, &base_repo)
            .await?;
        self.merge_pull_request_in_repo(
            candidate.number,
            &details.title,
            &details.body,
            &base_repo,
            &candidate.head_ref_oid,
        )
        .await?;
        Ok(base_ref)
    }

    async fn resolve_github_slug_for_remote(&self, remote: &str) -> Result<String> {
        let url = time::timeout(
            COMMAND_TIMEOUT,
            run_command(
                &self.git_binary,
                &["remote", "get-url", remote],
                Some(&self.git_root),
            ),
        )
        .await
        .map_err(|_| WisetreeError::other(format!("git remote get-url {remote} timed out")))?
        .map_err(WisetreeError::other)?;
        parse_github_slug(&url)
            .map(|(owner, repo)| format!("{owner}/{repo}"))
            .ok_or_else(|| WisetreeError::other(format!("Remote `{remote}` is not a GitHub URL.")))
    }

    fn finish_wise_merge(&self, number: u64, result: Result<String>) {
        self.wise_merge_in_flight
            .lock()
            .expect("wise_merge_in_flight poisoned")
            .remove(&number);
        match result {
            Ok(base_ref) => {
                self.wise_merge_merged
                    .lock()
                    .expect("wise_merge_merged poisoned")
                    .insert(number);
                self.mark_cached_pr_merged(number);
                self.send_dashboard_notice(DashboardNotice::success(format!(
                    "Wise Merge squash-merged PR #{number} after resolving base ref `{base_ref}`."
                )));
            }
            Err(err) => {
                self.wise_merge_failed_until
                    .lock()
                    .expect("wise_merge_failed_until poisoned")
                    .insert(number, Instant::now() + WISE_MERGE_FAILURE_BACKOFF);
                self.send_dashboard_notice(DashboardNotice::error(format!(
                    "Wise Merge failed for PR #{number}: {err}"
                )));
            }
        }
    }

    fn mark_cached_pr_merged(&self, number: u64) {
        let mut state = self.pr_state.lock().expect("pr_state poisoned");
        let mut changed = false;
        for entry in state.entries.values_mut() {
            let Some(pr) = entry.pull_request.as_mut() else {
                continue;
            };
            if pr.number == number && pr.state != PrState::Merged {
                pr.state = PrState::Merged;
                changed = true;
            }
        }
        if changed {
            state.dirty = true;
        }
    }

    fn send_dashboard_notice(&self, notice: DashboardNotice) {
        let tx = self
            .pr_state
            .lock()
            .expect("pr_state poisoned")
            .notice_tx
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(notice);
        }
    }

    /// Close a pull request via `gh pr close <number>`.
    pub async fn close_pull_request(&self, number: u64) -> Result<()> {
        self.require_gh()?;
        let number_arg = number.to_string();
        with_timeout(
            "gh pr close",
            PR_MERGE_TIMEOUT,
            run_command(
                &self.gh_binary,
                &["pr", "close", &number_arg],
                Some(&self.git_root),
            ),
        )
        .await?
        .map_err(WisetreeError::other)?;
        Ok(())
    }

    /// Drive the "Update Pull Request" pipeline against `worktree_path`.
    ///
    /// 1. `git fetch --all --prune`
    /// 2. Recheck behind count against `base_ref`; if zero → `AlreadyUpToDate`.
    /// 3. `git merge <base_ref>`.
    ///    - Exit 0 → push and return `MergedCleanly`.
    ///    - Non-zero → look for conflicts:
    ///       - If `ai.model` is blank → abort merge, return
    ///         `ConflictsRequireAi { conflicts }`.
    ///       - If `opencode` is missing → abort merge, return
    ///         `AiUnavailable { conflicts }`.
    ///       - Otherwise hand the worktree to opencode and return
    ///         `AiResolutionComplete` when it finishes — the commit + push
    ///         is deferred until the UI confirms via
    ///         `commit_and_push_ai_merge` / `abort_ai_merge`.
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
        let fetch = with_timeout(
            "git fetch",
            UPDATE_FETCH_TIMEOUT,
            run_command(&self.git_binary, &["fetch", "--all", "--prune"], Some(&cwd)),
        )
        .await?;
        if let Err(err) = fetch {
            return Ok(UpdatePullRequestOutcome::FetchFailed(err));
        }

        // 2. recheck behind
        let behind = behind_against_base(&self.git_binary, &cwd, base_ref).await;
        if matches!(behind, Some(0)) {
            // Local branch already contains all upstream changes. However
            // the user may have already merged upstream locally but never
            // pushed — in that case the PR on GitHub still shows "behind"
            // even though local HEAD is up to date. Check for unpushed
            // commits and push them before declaring AlreadyUpToDate.
            let ahead_of_origin = local_ahead_of_tracking(&self.git_binary, &cwd).await;
            if ahead_of_origin > 0 {
                send_phase(UpdatePhase::NoConflicts);
                send_phase(UpdatePhase::Pushing);
                let push = with_timeout(
                    "git push",
                    UPDATE_PUSH_TIMEOUT,
                    run_command(&self.git_binary, &["push", "origin", "HEAD"], Some(&cwd)),
                )
                .await?;
                if let Err(err) = push {
                    return Ok(UpdatePullRequestOutcome::PushFailed(err));
                }
                return Ok(UpdatePullRequestOutcome::Pushed);
            }
            send_phase(UpdatePhase::AlreadyUpToDate);
            return Ok(UpdatePullRequestOutcome::AlreadyUpToDate);
        }

        // 3. merge
        send_phase(UpdatePhase::Merging);
        let merge = with_timeout(
            "git merge",
            UPDATE_MERGE_TIMEOUT,
            run_command(&self.git_binary, &["merge", base_ref], Some(&cwd)),
        )
        .await?;

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
            // ai.update.model blank → no AI available, abort and let the UI
            // prompt the user to configure it or resolve conflicts manually.
            let model = self.config.ai.update.model.trim().to_string();
            if model.is_empty() {
                send_phase(UpdatePhase::ConflictsDetected {
                    count: conflicts.len(),
                });
                let _ = run_command(&self.git_binary, &["merge", "--abort"], Some(&cwd)).await;
                return Ok(UpdatePullRequestOutcome::ConflictsRequireAi { conflicts });
            }

            send_phase(UpdatePhase::ConflictsDetected {
                count: conflicts.len(),
            });

            // Bail early if opencode isn't on PATH so the user sees the
            // dedicated "install opencode" toast instead of a spawn
            // error from the PTY widget.
            if !binary_available(&self.opencode_binary) {
                let _ = run_command(&self.git_binary, &["merge", "--abort"], Some(&cwd)).await;
                return Ok(UpdatePullRequestOutcome::AiUnavailable { conflicts });
            }

            send_phase(UpdatePhase::AiResolving {
                model: model.clone(),
            });
            send_ai_activity(
                progress.as_ref(),
                AiActivityEvent::SessionStart {
                    model: model.clone(),
                },
            );

            // Build the command the UI will spawn inside its embedded
            // PTY. Invoke opencode's *default* TUI subcommand (no
            // explicit subcommand → `opencode [project]` starts the
            // full TUI) with `--prompt <prompt>` so the merger prompt
            // is auto-sent on launch and `-m <model>` so the user's
            // configured model is honored. `opencode run` would also
            // work, but its output is the plain CLI transcript — only
            // the TUI renders the full Monokai theme (orange Thinking
            // headers, colored tool calls, syntax-highlighted diffs)
            // the user expects to see inside the AI Activity panel. The
            // TUI has no `--variant` flag, so the configured reasoning
            // effort is seeded into opencode's `model.json` instead (see
            // `seed_opencode_tui_variant`).
            seed_opencode_tui_variant(&model, &self.config.ai.update.thinking);
            let prompt = build_merge_prompt(base_ref, &conflicts);
            let mut opencode_args: Vec<String> = vec![
                "--prompt".to_string(),
                prompt,
                "-m".to_string(),
                model.clone(),
            ];
            opencode_args.push(cwd.to_string_lossy().to_string());

            // Hand control to the UI. The merge is still mid-flight on
            // disk (conflict markers in the index); the screen owns the
            // PTY lifecycle from here, and the user finishes the flow
            // via `commit_and_push_ai_merge` or `abort_ai_merge`.
            return Ok(UpdatePullRequestOutcome::ConflictsHandedOffToUi {
                opencode_binary: self.opencode_binary.clone(),
                opencode_args,
                cwd: cwd.clone(),
                model,
                base_ref: base_ref.to_string(),
                conflicts,
            });
        }

        // 4. push (clean merge path only — AI merges return above for review)
        send_phase(UpdatePhase::NoConflicts);
        send_phase(UpdatePhase::Pushing);
        let push = with_timeout(
            "git push",
            UPDATE_PUSH_TIMEOUT,
            run_command(&self.git_binary, &["push", "origin", "HEAD"], Some(&cwd)),
        )
        .await?;
        if let Err(err) = push {
            return Ok(UpdatePullRequestOutcome::PushFailed(err));
        }

        Ok(UpdatePullRequestOutcome::MergedCleanly)
    }

    /// Push-only counterpart to the update pipeline: just runs
    /// `git push origin HEAD` against `worktree_path`. Powers the dashboard's
    /// "Push Pull Request" action (for branches that are ahead-but-not-behind,
    /// e.g. a local merge that never got pushed) and the Terminal recovery
    /// panel's "Accept" re-push. Returns `Pushed` on success or `PushFailed`
    /// on failure — the exact same failure variant the merge pipeline emits,
    /// so both paths hand off to the same recovery UI.
    pub async fn push_pull_request_with_progress(
        &self,
        worktree_path: &str,
        progress: Option<mpsc::UnboundedSender<UpdateProgress>>,
    ) -> Result<UpdatePullRequestOutcome> {
        let cwd = PathBuf::from(worktree_path);
        if let Some(tx) = progress.as_ref() {
            let _ = tx.send(UpdateProgress::Phase(UpdatePhase::Pushing));
        }
        let push = with_timeout(
            "git push",
            UPDATE_PUSH_TIMEOUT,
            run_command(&self.git_binary, &["push", "origin", "HEAD"], Some(&cwd)),
        )
        .await?;
        match push {
            Ok(_) => Ok(UpdatePullRequestOutcome::Pushed),
            Err(err) => Ok(UpdatePullRequestOutcome::PushFailed(err)),
        }
    }

    /// Fetch the remote and merge the worktree at `worktree_path` with
    /// the first reachable ref in `BASE_REF_PRIORITY` (upstream/main →
    /// upstream/master → origin/main → origin/master). Powers the
    /// dashboard's "Update branch (locally)" action on any worktree —
    /// mother or derived — pulling its base branch in without pushing.
    pub async fn update_branch(&self, worktree_path: &str) -> Result<UpdateBranchOutcome> {
        let cwd = PathBuf::from(worktree_path);

        // Pre-flight: a dirty working tree makes `git merge` refuse before it
        // even starts ("Your local changes ... would be overwritten by
        // merge"). That's not a merge conflict — no markers land, so there is
        // nothing for opencode to resolve — so we catch it here and tell the
        // user to commit or stash first instead of surfacing a raw git
        // refusal that looks like (but isn't) a conflict.
        let dirty = dirty_tracked_files(&self.git_binary, &cwd).await;
        if !dirty.is_empty() {
            return Ok(UpdateBranchOutcome::WorkingTreeDirty { files: dirty });
        }

        let fetch = with_timeout(
            "git fetch",
            UPDATE_FETCH_TIMEOUT,
            run_command(&self.git_binary, &["fetch", "--all", "--prune"], Some(&cwd)),
        )
        .await?;
        if let Err(err) = fetch {
            return Ok(UpdateBranchOutcome::FetchFailed(err));
        }

        let Some(base_ref) = resolve_base_ref_with_binary(&self.git_binary, &cwd, None).await
        else {
            return Ok(UpdateBranchOutcome::NoBaseRef);
        };

        let merge = with_timeout(
            "git merge",
            UPDATE_MERGE_TIMEOUT,
            run_command(&self.git_binary, &["merge", &base_ref], Some(&cwd)),
        )
        .await?;

        let stderr = match merge {
            Ok(stdout) => return Ok(classify_merge_output(base_ref, &stdout)),
            Err(stderr) => stderr,
        };

        // The merge failed. Distinguish genuine conflicts (which we can
        // hand to opencode) from other failures (dirty tree, refusal),
        // mirroring the conflict handling in
        // `update_pull_request_with_progress`.
        let conflicts = conflicted_files(&self.git_binary, &cwd).await;
        if conflicts.is_empty() {
            return Ok(UpdateBranchOutcome::MergeFailed {
                base_ref,
                message: stderr,
            });
        }

        // ai.update.model blank → no AI available: abort the merge and let the
        // UI prompt the user to configure it or resolve manually.
        let model = self.config.ai.update.model.trim().to_string();
        if model.is_empty() {
            let _ = run_command(&self.git_binary, &["merge", "--abort"], Some(&cwd)).await;
            return Ok(UpdateBranchOutcome::ConflictsRequireAi { conflicts });
        }

        // Bail early if opencode isn't on PATH so the user sees the
        // dedicated "install opencode" toast instead of a spawn error.
        if !binary_available(&self.opencode_binary) {
            let _ = run_command(&self.git_binary, &["merge", "--abort"], Some(&cwd)).await;
            return Ok(UpdateBranchOutcome::AiUnavailable { conflicts });
        }

        // Hand control to the UI. The merge is still mid-flight on disk
        // (conflict markers in the index); the screen owns the opencode
        // PTY lifecycle from here and commits the result locally (no push)
        // via the same machinery as the Update Pull Request flow.
        // The TUI takes no `--variant`; seed the reasoning effort into
        // opencode's `model.json` so it opens at the configured strength.
        seed_opencode_tui_variant(&model, &self.config.ai.update.thinking);
        let prompt = build_merge_prompt(&base_ref, &conflicts);
        let mut opencode_args: Vec<String> = vec![
            "--prompt".to_string(),
            prompt,
            "-m".to_string(),
            model.clone(),
        ];
        opencode_args.push(cwd.to_string_lossy().to_string());
        Ok(UpdateBranchOutcome::ConflictsHandedOffToUi {
            opencode_binary: self.opencode_binary.clone(),
            opencode_args,
            cwd: cwd.clone(),
            model,
            base_ref,
            conflicts,
        })
    }

    /// Commit the AI-resolved files (`git add -A` + `git commit`) and push
    /// to the first reachable remote in `upstream → origin`. Called after
    /// the user clicks **Complete** in the AI Activity panel.
    pub async fn commit_and_push_ai_merge(
        &self,
        worktree_path: &str,
        base_ref: &str,
        model: &str,
    ) -> Result<UpdatePullRequestOutcome> {
        let cwd = PathBuf::from(worktree_path);

        if let Err(err) = run_command(&self.git_binary, &["add", "-A"], Some(&cwd)).await {
            return Ok(UpdatePullRequestOutcome::MergeFailed(format!(
                "git add failed: {err}"
            )));
        }
        let title = crate::constants::UPDATE_MERGE_COMMIT_MESSAGE;
        let description =
            format!("Merged `{base_ref}` and resolved conflicts using opencode ({model}).");
        let message = format!("{title}\n\n{description}");
        if let Err(err) =
            run_command(&self.git_binary, &["commit", "-m", &message], Some(&cwd)).await
        {
            return Ok(UpdatePullRequestOutcome::MergeFailed(format!(
                "git commit failed: {err}"
            )));
        }

        // Push fallback: upstream HEAD → origin HEAD.
        let mut errors: Vec<String> = Vec::new();
        for remote in ["upstream", "origin"] {
            let push = with_timeout(
                "git push",
                UPDATE_PUSH_TIMEOUT,
                run_command(&self.git_binary, &["push", remote, "HEAD"], Some(&cwd)),
            )
            .await?;
            match push {
                Ok(_) => return Ok(UpdatePullRequestOutcome::MergedWithAiResolution),
                Err(err) => errors.push(format!("{remote}: {err}")),
            }
        }
        Ok(UpdatePullRequestOutcome::PushFailed(errors.join(" | ")))
    }

    /// Abort the in-progress merge (`git merge --abort`) after the user
    /// clicks **Cancel** in the AI Activity panel. Restores the worktree
    /// to its pre-merge state.
    pub async fn abort_ai_merge(&self, worktree_path: &str) -> Result<UpdatePullRequestOutcome> {
        let cwd = PathBuf::from(worktree_path);
        let abort = run_command(&self.git_binary, &["merge", "--abort"], Some(&cwd)).await;
        match abort {
            Ok(_) => Ok(UpdatePullRequestOutcome::DiscardedAiMerge),
            Err(err) => Ok(UpdatePullRequestOutcome::AbortFailed(err)),
        }
    }

    /// Read-only preparation for the "Enrich Pull Request" flow. Gathers the
    /// commit log + diff against `base_ref`, extracts the ticket from the
    /// branch name, reads the repo's PR template (falling back to the
    /// embedded one), and renders `prompts/enricher.md` into the opencode
    /// command the UI will spawn. No git mutation happens here — the AI's
    /// only job is to write `pull_request.md`.
    pub async fn prepare_enrich(
        &self,
        worktree_path: &str,
        branch: &str,
        base_ref: &str,
    ) -> Result<EnrichPreparation> {
        let cwd = PathBuf::from(worktree_path);

        // Commit log (oldest first) and full diff against the base ref.
        let log_range = format!("{base_ref}..HEAD");
        let diff_range = format!("{base_ref}...HEAD");
        let git_log = with_timeout(
            "git log",
            ENRICH_CONTEXT_TIMEOUT,
            run_command(
                &self.git_binary,
                &["log", &log_range, "--reverse", "--format=### %s%n%n%b"],
                Some(&cwd),
            ),
        )
        .await?
        .unwrap_or_default();
        let git_diff = with_timeout(
            "git diff",
            ENRICH_CONTEXT_TIMEOUT,
            run_command(&self.git_binary, &["diff", &diff_range], Some(&cwd)),
        )
        .await?
        .unwrap_or_default();

        if git_diff.trim().is_empty() && git_log.trim().is_empty() {
            return Ok(EnrichPreparation::NothingToDescribe);
        }

        let model = self.config.ai.enrich.model.trim().to_string();
        if model.is_empty() {
            return Ok(EnrichPreparation::AiNotConfigured);
        }
        if !binary_available(&self.opencode_binary) {
            return Ok(EnrichPreparation::AiUnavailable);
        }

        let ticket = extract_ticket(branch).unwrap_or_default();
        let template = read_pr_template(&cwd).await;
        let prompt = build_enrich_prompt(base_ref, branch, &ticket, &git_log, &git_diff, &template);

        // Invoke opencode's default TUI (no subcommand) with `--prompt` so
        // the enricher instructions auto-send on launch and `-m <model>` so
        // the user's configured model is honored — mirrors the merge flow.
        // The TUI takes no `--variant`, so the reasoning effort is seeded into
        // opencode's `model.json` first (see `seed_opencode_tui_variant`).
        seed_opencode_tui_variant(&model, &self.config.ai.enrich.thinking);
        let mut opencode_args: Vec<String> = vec![
            "--prompt".to_string(),
            prompt,
            "-m".to_string(),
            model.clone(),
        ];
        opencode_args.push(cwd.to_string_lossy().to_string());

        Ok(EnrichPreparation::HandedOffToUi {
            opencode_binary: self.opencode_binary.clone(),
            opencode_args,
            cwd,
            model,
        })
    }

    /// Open or update the pull request from the drafted title + body. When
    /// `number` is `Some`, the existing PR's title/body are edited (with any
    /// media from the old body re-inserted under `# Overview`). When `None`,
    /// the branch is pushed and a new PR is created.
    /// Open or update the pull request described by `params`. Labels are
    /// applied via `--label`/`--add-label`; the PR is always self-assigned
    /// via `--assignee "@me"` / `--add-assignee "@me"`.
    pub async fn submit_pull_request(
        &self,
        params: &EnrichSubmitRequest,
        activity: Option<&mpsc::UnboundedSender<(String, ActivityKind)>>,
    ) -> Result<EnrichSubmitOutcome> {
        let EnrichSubmitRequest {
            worktree_path,
            branch,
            number,
            base_ref,
            title,
            body,
            labels,
            existing_title,
            existing_labels,
        } = params;
        self.require_gh()?;
        let cwd = PathBuf::from(worktree_path);

        let emit = |text: &str| {
            if let Some(tx) = activity {
                let _ = tx.send((text.to_string(), ActivityKind::Status));
            }
        };

        match number {
            // Update the existing PR's description, preserving any media
            // (screenshots / videos) GitHub already had in the old body.
            Some(number) => {
                let body = match self.fetch_pr_details(*number).await {
                    Ok(details) => preserve_media(&details.body, body),
                    // If we can't read the old body, push the new body as-is
                    // rather than blocking the update entirely.
                    Err(_) => body.to_string(),
                };
                let number_arg = number.to_string();
                let skip_title = existing_title.as_ref().is_some_and(|t| !t.is_empty());
                let skip_labels = !existing_labels.is_empty();
                let emit_title = if skip_title { "(skipped)" } else { "<title>" };
                let emit_labels = if skip_labels { " (labels skipped)" } else { "" };
                emit(&format!(
                    "$ gh pr edit #{number} --title {emit_title} --body <body> --add-assignee @me{emit_labels}"
                ));
                let mut edit_args: Vec<String> =
                    vec!["pr".into(), "edit".into(), number_arg.clone()];
                if !skip_title {
                    edit_args.push("--title".into());
                    edit_args.push(title.into());
                }
                edit_args.extend([
                    "--body".into(),
                    body.clone(),
                    "--add-assignee".into(),
                    "@me".into(),
                ]);
                if !skip_labels {
                    for label in labels {
                        edit_args.push("--add-label".into());
                        edit_args.push(label.clone());
                    }
                }
                let edit_args_ref: Vec<&str> = edit_args.iter().map(String::as_str).collect();
                let edit_result = with_timeout(
                    "gh pr edit",
                    ENRICH_SUBMIT_TIMEOUT,
                    run_command_streamed(&self.gh_binary, &edit_args_ref, Some(&cwd), activity),
                )
                .await?;
                match edit_result {
                    Ok(_) => Ok(EnrichSubmitOutcome::Updated { number: *number }),
                    Err(err) => Ok(EnrichSubmitOutcome::SubmitFailed(err)),
                }
            }
            // Create a brand-new PR: push the branch, then `gh pr create`.
            None => {
                emit(&format!("$ git push -u origin {branch}"));
                let push = with_timeout(
                    "git push",
                    ENRICH_PUSH_TIMEOUT,
                    run_command_streamed(
                        &self.git_binary,
                        &["push", "-u", "origin", branch],
                        Some(&cwd),
                        activity,
                    ),
                )
                .await?;
                if let Err(err) = push {
                    return Ok(EnrichSubmitOutcome::PushFailed(err));
                }

                // `--head owner:branch` keeps `gh` from aborting when the
                // worktree has uncommitted files; fall back to the bare
                // branch if the owner lookup fails.
                // Parse owner from `origin` URL so that forks point to the
                // correct user (not the upstream org that `gh repo view` returns).
                let origin_url = run_command(
                    &self.git_binary,
                    &["remote", "get-url", "origin"],
                    Some(&cwd),
                )
                .await
                .ok();
                let owner = origin_url
                    .as_deref()
                    .and_then(parse_github_slug)
                    .map(|(o, _)| o);
                let head = match owner {
                    Some(owner) => format!("{owner}:{branch}"),
                    None => branch.to_string(),
                };
                // The PR targets the branch the worktree was cut from — the
                // remote-tracking prefix (`upstream/`, `origin/`) is stripped
                // to the bare branch name `gh pr create --base` expects. When
                // the base is unresolved we omit `--base` and let `gh` use the
                // repo default, preserving the prior behavior.
                let base_branch = base_ref
                    .as_deref()
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .map(|b| branch_name_from_ref(b).to_string());
                let base_display = base_branch
                    .as_deref()
                    .map(|b| format!(" --base {b}"))
                    .unwrap_or_default();
                emit(&format!(
                    "$ gh pr create --title <title> --body <body> --head {head}{base_display} --assignee @me"
                ));
                let mut create_args: Vec<String> = vec![
                    "pr".into(),
                    "create".into(),
                    "--title".into(),
                    title.into(),
                    "--body".into(),
                    body.to_string(),
                    "--head".into(),
                    head.clone(),
                ];
                if let Some(base_branch) = base_branch.as_deref() {
                    create_args.push("--base".into());
                    create_args.push(base_branch.into());
                }
                create_args.extend(["--assignee".into(), "@me".into()]);
                for label in labels {
                    create_args.push("--label".into());
                    create_args.push(label.clone());
                }
                let create_args_ref: Vec<&str> = create_args.iter().map(String::as_str).collect();
                let create = with_timeout(
                    "gh pr create",
                    ENRICH_SUBMIT_TIMEOUT,
                    run_command_streamed(&self.gh_binary, &create_args_ref, Some(&cwd), activity),
                )
                .await?;
                match create {
                    Ok(out) => {
                        let url = pr_url_from_output(&out);
                        let number = pr_number_from_url(&url).unwrap_or(0);
                        Ok(EnrichSubmitOutcome::Created { number, url })
                    }
                    Err(err) => Ok(EnrichSubmitOutcome::SubmitFailed(err)),
                }
            }
        }
    }

    // ── "Fix Pull Request" pipeline ────────────────────────────────────
    //
    // Resolve PR review comments. Deterministic work (sync, fetch, filter,
    // group, commit, reply, push) is Rust; the AI is invoked only to judge +
    // plan each comment (captured) and to apply an approved fix (live PTY).

    /// Sync the PR branch, then fetch, filter, and group its review comments.
    ///
    /// No AI here. The branch is already checked out in this worktree (that's
    /// why "Fix" is offered), so we sync it with a fast-forward-only pull
    /// rather than `gh pr checkout`, which could switch branches inside the
    /// worktree. Resolved and minimized threads are dropped, as are outdated
    /// threads we already replied to; surviving inline comments are grouped by
    /// file + line, and PR-level review summaries are folded into one group.
    pub async fn prepare_fix(&self, worktree_path: &str, number: u64) -> Result<FixPreparation> {
        if !self.gh_available {
            return Ok(FixPreparation::GhUnavailable);
        }
        // The pipeline's first AI step is planning, so gate on `fix.plan`;
        // the apply step validates `fix.apply.model` itself when it runs.
        let model = self.config.ai.fix.plan.model.trim().to_string();
        if model.is_empty() {
            return Ok(FixPreparation::AiNotConfigured);
        }
        if !binary_available(&self.opencode_binary) {
            return Ok(FixPreparation::AiUnavailable);
        }
        let cwd = PathBuf::from(worktree_path);

        // Sync the branch with its upstream so fixes land on the latest PR
        // state and the final push updates the branch reviewers see.
        let pull = time::timeout(
            FIX_SYNC_TIMEOUT,
            run_command(&self.git_binary, &["pull", "--ff-only"], Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("git pull --ff-only timed out after 60s"))?;
        if let Err(err) = pull {
            return Ok(FixPreparation::SyncFailed(err));
        }

        // The PR may not live on `origin`: a fork opens PRs against its
        // upstream, so `origin` (the fork) has no such PR number. Ask `gh`
        // which repo the PR was actually opened against — `gh pr view`
        // resolves the base repo from the local remotes, forks included —
        // and read the slug off the returned URL so the GraphQL fetch and
        // replies hit the right repo. `headRepository*` fields point at the
        // (possibly fork) head and would misdirect both.
        let number_arg = number.to_string();
        let view = time::timeout(
            FIX_FETCH_TIMEOUT,
            run_gh_command(
                &self.gh_binary,
                &["pr", "view", &number_arg, "--json", "url"],
                Some(&cwd),
            ),
        )
        .await
        .map_err(|_| WisetreeError::other("gh pr view timed out after 15s"))?;
        let view = match view {
            Ok(out) => out,
            Err(err) => return Ok(FixPreparation::SyncFailed(err)),
        };
        let Some((owner, repo)) = parse_pr_repo_json(&view) else {
            return Ok(FixPreparation::SyncFailed(
                "could not resolve the PR's repository from gh pr view output.".to_string(),
            ));
        };

        // One GraphQL call returns every inline review thread (with the
        // resolved/outdated/minimized flags we filter on) plus every PR-level
        // review summary body, which is folded into its own group.
        let query = build_fix_feedback_query(&owner, &repo, number);
        let arg = format!("query={query}");
        let output = time::timeout(
            FIX_FETCH_TIMEOUT,
            run_gh_command(&self.gh_binary, &["api", "graphql", "-f", &arg], Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("gh api graphql timed out after 15s"))?
        .map_err(WisetreeError::other)?;

        let groups = parse_and_group_review_feedback(&output).map_err(WisetreeError::other)?;
        if groups.is_empty() {
            return Ok(FixPreparation::NoComments);
        }
        Ok(FixPreparation::Ready {
            groups,
            owner,
            repo,
        })
    }

    /// Judge + plan one comment group with a single captured (non-interactive)
    /// opencode call. The planning output is structured text we parse in Rust;
    /// the AI must not edit any file in this phase. When `feedback` is set the
    /// user chose "Other" — the previous plan + their feedback are threaded
    /// back in so the model revises rather than starts over. `history` carries
    /// the comments / replies / fixes already resolved earlier in this run so
    /// the model can interpret a comment that refers back to them.
    pub async fn plan_comment(
        &self,
        worktree_path: &str,
        group: &CommentGroup,
        feedback: Option<&str>,
        previous_plan: Option<&str>,
        history: Option<&str>,
    ) -> Result<FixVerdict> {
        let cwd = PathBuf::from(worktree_path);
        let model = self.config.ai.fix.plan.model.trim().to_string();
        if model.is_empty() {
            return Err(WisetreeError::other("ai.fix.plan.model is not configured."));
        }
        let code = match &group.file {
            Some(file) => read_code_window(&cwd, file, group.line).await,
            None => String::new(),
        };
        let prompt = build_fix_plan_prompt(group, &code, feedback, previous_plan, history);
        // `opencode run` is the captured/non-interactive transcript mode — no
        // inner TUI; we parse its stdout. `-m` honors the configured model.
        // `--agent plan` pins it to opencode's read-only Plan agent (write /
        // edit / patch tools disabled), so this phase can ONLY think and emit a
        // verdict — it cannot touch files. This matters most on the "Other"
        // re-plan: the user's feedback reads like a direct instruction, and the
        // default (build) agent would act on it — editing the file and skipping
        // the verdict — which made "Other" silently apply a change and advance
        // instead of showing a revised plan. Read-only forces a clean re-plan.
        // `opencode run` accepts `--variant <effort>`, so the plan phase honors
        // its configured thinking strength directly (unlike the TUI flows).
        let mut run_args: Vec<String> = vec![
            "run".to_string(),
            prompt,
            "-m".to_string(),
            model.clone(),
            "--agent".to_string(),
            "plan".to_string(),
        ];
        run_args.extend(run_variant_args(&self.config.ai.fix.plan.thinking));
        let run_args_ref: Vec<&str> = run_args.iter().map(String::as_str).collect();
        let output = time::timeout(
            FIX_PLAN_TIMEOUT,
            run_command(&self.opencode_binary, &run_args_ref, Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("opencode planning timed out after 180s"))?
        .map_err(WisetreeError::other)?;

        parse_fix_verdict(&output).ok_or_else(|| {
            WisetreeError::other("could not parse a verdict from the planning AI output.")
        })
    }

    /// Build the spawn parameters for the live apply phase. The targeted
    /// file(s) are edited live inside the opencode PTY (the AI Activity
    /// panel), exactly like the merge conflict-resolution flow — opencode
    /// reads the files itself, so nothing is embedded in the prompt but the
    /// comment + approved plan.
    pub async fn prepare_apply(
        &self,
        worktree_path: &str,
        group: &CommentGroup,
        plan: &FixPlan,
    ) -> Result<FixApplyHandoff> {
        let cwd = PathBuf::from(worktree_path);
        let model = self.config.ai.fix.apply.model.trim().to_string();
        if model.is_empty() {
            return Err(WisetreeError::other(
                "ai.fix.apply.model is not configured.",
            ));
        }
        if !binary_available(&self.opencode_binary) {
            return Err(WisetreeError::other("opencode CLI is not on PATH."));
        }
        let prompt = build_fix_apply_prompt(group, plan);
        // The apply phase runs in the opencode TUI (live edits in the AI
        // Activity panel), which takes no `--variant`; seed the reasoning
        // effort into `model.json` before spawning.
        seed_opencode_tui_variant(&model, &self.config.ai.fix.apply.thinking);
        let mut opencode_args: Vec<String> =
            vec!["--prompt".to_string(), prompt, "-m".to_string(), model];
        opencode_args.push(cwd.to_string_lossy().to_string());
        Ok(FixApplyHandoff {
            opencode_binary: self.opencode_binary.clone(),
            opencode_args,
            cwd,
        })
    }

    /// After a live apply: stage the change, commit it with the review
    /// commit-message format, and reply to the reviewer with the commit link.
    /// All deterministic — no AI. `comment_index` is the 1-based position in
    /// processing order, used in the commit body.
    ///
    /// opencode sometimes concludes the code already satisfies the comment and
    /// edits nothing. That is not a failure: when the stage is empty we skip
    /// the commit and instead reply that the comment is already addressed,
    /// returning [`FixCommitOutcome::AlreadyResolved`].
    #[allow(clippy::too_many_arguments)]
    pub async fn commit_and_reply(
        &self,
        worktree_path: &str,
        owner: &str,
        repo: &str,
        number: u64,
        pr_url: &str,
        comment_index: usize,
        group: &CommentGroup,
        plan: &FixPlan,
    ) -> Result<FixCommitOutcome> {
        let cwd = PathBuf::from(worktree_path);

        // Stage only what this fix touched: the targeted file when known. A
        // PR-level fix has no single anchor file, so fall back to `add -u`
        // (tracked modifications only) — never `-A`, which would sweep stray
        // untracked files (a leftover `pull_request.md`, `.DS_Store`, …) into
        // the review-fix commit.
        let stage_args: Vec<&str> = match &group.file {
            Some(file) => vec!["add", "--", file.as_str()],
            None => vec!["add", "-u"],
        };
        time::timeout(
            FIX_COMMIT_TIMEOUT,
            run_command(&self.git_binary, &stage_args, Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("git add timed out"))?
        .map_err(WisetreeError::other)?;

        // If nothing got staged, the apply produced no change — opencode judged
        // the code already handles the comment. Tell the reviewer so, and stop
        // here: there is nothing to commit, and that's a valid resolution, not
        // an error that should abort the loop.
        let staged = time::timeout(
            FIX_COMMIT_TIMEOUT,
            run_command(
                &self.git_binary,
                &["diff", "--cached", "--name-only"],
                Some(&cwd),
            ),
        )
        .await
        .map_err(|_| WisetreeError::other("git diff --cached timed out"))?
        .map_err(WisetreeError::other)?;
        if staged.trim().is_empty() {
            post_reply_internal(
                &self.gh_binary,
                &cwd,
                owner,
                repo,
                number,
                group,
                ALREADY_RESOLVED_REPLY,
            )
            .await
            .map_err(|err| {
                WisetreeError::other(format!(
                    "the apply step produced no change (the code already addresses the \
                     comment), but posting the reply failed: {err}"
                ))
            })?;
            return Ok(FixCommitOutcome::AlreadyResolved);
        }

        let (subject, body) = format_commit_message(number, comment_index, &group.brief(), plan);
        let commit = time::timeout(
            FIX_COMMIT_TIMEOUT,
            run_command(
                &self.git_binary,
                &["commit", "-m", &subject, "-m", &body],
                Some(&cwd),
            ),
        )
        .await
        .map_err(|_| WisetreeError::other("git commit timed out"))?;
        if let Err(err) = commit {
            return Err(WisetreeError::other(if err.trim().is_empty() {
                "git commit failed after staging the change.".to_string()
            } else {
                err
            }));
        }

        let full_hash = run_command(&self.git_binary, &["rev-parse", "HEAD"], Some(&cwd))
            .await
            .unwrap_or_default();
        let commit_url = if pr_url.is_empty() {
            format!("https://github.com/{owner}/{repo}/pull/{number}/changes/{full_hash}")
        } else {
            format!("{}/changes/{full_hash}", pr_url.trim_end_matches('/'))
        };
        let reply = format_reply(&commit_url, plan);

        match post_reply_internal(&self.gh_binary, &cwd, owner, repo, number, group, &reply).await {
            Ok(_) => Ok(FixCommitOutcome::Committed),
            Err(err) => Err(WisetreeError::other(format!(
                "committed {} but failed to post the reply: {err}",
                short_hash(&full_hash)
            ))),
        }
    }

    /// Post a non-actionable reply (the `reply` verdict) without any commit.
    pub async fn post_reply(
        &self,
        worktree_path: &str,
        owner: &str,
        repo: &str,
        number: u64,
        group: &CommentGroup,
        text: &str,
    ) -> Result<()> {
        let cwd = PathBuf::from(worktree_path);
        post_reply_internal(&self.gh_binary, &cwd, owner, repo, number, group, text)
            .await
            .map(|_| ())
            .map_err(WisetreeError::other)
    }

    /// Add a 😄 ("laugh") reaction to the comment that praised us — the
    /// reviewer's most recent comment in `group`, located by
    /// [`CommentGroup::praise_reaction_target_id`], not the first comment (which
    /// is usually the original change request). Skips silently when no reviewer
    /// comment carries an id (e.g. PR-level summary comments have no inline
    /// anchor). The GitHub reactions API is idempotent — re-adding an existing
    /// reaction returns 200 without a duplicate, so no pre-check is needed.
    pub async fn react_to_praise_comment(
        &self,
        worktree_path: &str,
        owner: &str,
        repo: &str,
        group: &CommentGroup,
    ) -> Result<()> {
        let Some(comment_id) = group.praise_reaction_target_id() else {
            return Ok(());
        };
        let cwd = PathBuf::from(worktree_path);
        let endpoint = format!("repos/{owner}/{repo}/pulls/comments/{comment_id}/reactions");
        time::timeout(
            FIX_REPLY_TIMEOUT,
            run_command(
                &self.gh_binary,
                &["api", "--method", "POST", &endpoint, "-f", "content=laugh"],
                Some(&cwd),
            ),
        )
        .await
        .map_err(|_| WisetreeError::other("gh react timed out"))?
        .map_err(WisetreeError::other)?;
        Ok(())
    }

    /// Final step of the Fix loop: push every review-fix commit to origin so
    /// the commit links in the replies resolve.
    pub async fn push_fix(&self, worktree_path: &str) -> Result<()> {
        let cwd = PathBuf::from(worktree_path);
        time::timeout(
            FIX_PUSH_TIMEOUT,
            run_command(&self.git_binary, &["push", "origin", "HEAD"], Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("git push timed out after 60s"))?
        .map_err(WisetreeError::other)?;
        Ok(())
    }

    // ── "Review Pull Request" pipeline ─────────────────────────────────
    //
    // AI is called in exactly one place: the captured per-file scan (plus
    // its "Other" revision variant). Everything else — PR lookup, diff
    // fetch + per-file split, line-number validation, comment posting, the
    // review-summary template, and the final `gh pr review` submission —
    // is deterministic code below.

    /// Deterministic Review preparation: gates (gh / model / opencode),
    /// branch sync, PR lookup (owner/repo/head sha), `gh pr diff` split per
    /// changed file, and the existing-comments dedup context.
    pub async fn prepare_review(
        &self,
        worktree_path: &str,
        number: u64,
    ) -> Result<ReviewPreparation> {
        if !self.gh_available {
            return Ok(ReviewPreparation::GhUnavailable);
        }
        if self.config.ai.review.model.trim().is_empty() {
            return Ok(ReviewPreparation::AiNotConfigured);
        }
        if !binary_available(&self.opencode_binary) {
            return Ok(ReviewPreparation::AiUnavailable);
        }
        let cwd = PathBuf::from(worktree_path);

        // Sync the branch with its upstream so the AI reads worktree files
        // matching the PR head it is reviewing.
        let pull = time::timeout(
            REVIEW_SYNC_TIMEOUT,
            run_command(&self.git_binary, &["pull", "--ff-only"], Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("git pull --ff-only timed out after 60s"))?;
        if let Err(err) = pull {
            return Ok(ReviewPreparation::SyncFailed(err));
        }

        // Resolve the repo the PR was opened against (forks included) and
        // its head sha — the inline-comment API needs `commit_id`.
        let number_arg = number.to_string();
        let view = time::timeout(
            REVIEW_FETCH_TIMEOUT,
            run_gh_command(
                &self.gh_binary,
                &[
                    "pr",
                    "view",
                    &number_arg,
                    "--json",
                    "url,headRefOid,baseRefName",
                ],
                Some(&cwd),
            ),
        )
        .await
        .map_err(|_| WisetreeError::other("gh pr view timed out after 30s"))?;
        let view = match view {
            Ok(out) => out,
            Err(err) => return Ok(ReviewPreparation::SyncFailed(err)),
        };
        let Some((owner, repo, head_sha, base_ref_name)) = parse_review_pr_json(&view) else {
            return Ok(ReviewPreparation::SyncFailed(
                "could not resolve the PR's repository and head sha from gh pr view output."
                    .to_string(),
            ));
        };

        // `gh pr diff` returns the diff exactly as GitHub renders it, so the
        // new-side line numbers we extract match the lines GitHub accepts
        // inline comments on. GitHub's diff endpoint, though, returns a
        // persistent 5xx when a PR's diff is too large for its service to
        // generate; retrying can't help. In that case fall back to
        // reproducing the same three-dot diff locally from the synced branch,
        // which has no size limit.
        let gh_diff = time::timeout(
            REVIEW_FETCH_TIMEOUT,
            run_command(&self.gh_binary, &["pr", "diff", &number_arg], Some(&cwd)),
        )
        .await
        .unwrap_or_else(|_| Err("gh pr diff timed out after 30s".to_string()));
        let diff = match gh_diff {
            Ok(diff) => diff,
            Err(gh_err) => self
                .local_pr_diff(&cwd, &base_ref_name)
                .await
                .map_err(|local_err| {
                    WisetreeError::other(format!(
                        "gh pr diff failed ({gh_err}); local diff fallback also failed \
                         ({local_err})"
                    ))
                })?,
        };
        let parsed = parse_review_diff(&diff);
        if parsed.is_empty() {
            return Ok(ReviewPreparation::NoChanges);
        }
        // Lockfiles, minified bundles, snapshots, … are excluded here, before
        // any AI call — they still reach the screen so the final report can
        // show each one with its reason.
        let (mut files, skipped) = partition_reviewable_files(parsed);

        // Existing review comments, grouped per file — dedup context only,
        // so a fetch failure just means the AI scans without it.
        let endpoint = format!("repos/{owner}/{repo}/pulls/{number}/comments?per_page=100");
        if let Ok(Ok(json)) = time::timeout(
            REVIEW_FETCH_TIMEOUT,
            run_command(&self.gh_binary, &["api", &endpoint], Some(&cwd)),
        )
        .await
        {
            let by_path = parse_existing_review_comments(&json);
            for file in &mut files {
                if let Some(existing) = by_path.get(&file.path) {
                    file.existing_comments = existing.rendered.clone();
                    file.existing_keys = existing.keys.clone();
                }
            }
        }

        Ok(ReviewPreparation::Ready {
            files,
            skipped,
            owner,
            repo,
            head_sha,
        })
    }

    /// Reproduce a PR's diff locally when `gh pr diff` is unavailable —
    /// notably when GitHub's diff endpoint returns a persistent 5xx because a
    /// PR's diff is too large for its service to generate. GitHub renders a
    /// PR's "Files changed" as a three-dot diff (`base...head`, i.e. from the
    /// merge-base to the head), and the PR branch is already checked out and
    /// fast-forwarded in this worktree, so `git diff <base>...HEAD` reproduces
    /// it. The new-side line numbers match GitHub's regardless — they are
    /// intrinsic to the head file contents, not to how GitHub renders them —
    /// so the anchors we later post inline stay valid. Unlike the API, local
    /// git has no diff-size limit.
    async fn local_pr_diff(
        &self,
        cwd: &Path,
        base_ref_name: &str,
    ) -> std::result::Result<String, String> {
        // Map GitHub's base branch onto a local remote-tracking ref (falls
        // back to the branch's own base when the name is unknown).
        let base_ref = resolve_base_ref_with_binary(&self.git_binary, cwd, Some(base_ref_name))
            .await
            .ok_or_else(|| {
                format!(
                    "could not resolve a local base ref for the PR's base branch \
                     '{base_ref_name}'"
                )
            })?;
        // Refresh the base so its merge-base with HEAD matches GitHub's
        // current base tip, and thus the exact set of changed lines.
        if let Some((remote, branch)) = base_ref.split_once('/') {
            let refspec = format!("+{branch}:refs/remotes/{remote}/{branch}");
            let _ = time::timeout(
                REVIEW_FETCH_TIMEOUT,
                run_command(&self.git_binary, &["fetch", remote, &refspec], Some(cwd)),
            )
            .await;
        }
        let range = format!("{base_ref}...HEAD");
        time::timeout(
            REVIEW_FETCH_TIMEOUT,
            run_command(&self.git_binary, &["diff", "--no-color", &range], Some(cwd)),
        )
        .await
        .map_err(|_| "git diff for the local PR-diff fallback timed out".to_string())?
    }

    /// Scan one changed file with a single captured (non-interactive)
    /// opencode call and parse its findings. Test files get the dedicated
    /// test-quality prompt profile; everything else the source profile. The
    /// AI only reads and emits structured text — `--agent plan` disables
    /// every write tool. When
    /// `feedback` is set the user chose "Other" on a finding: the previous
    /// finding + their feedback are threaded back in so the model revises
    /// exactly one finding rather than re-scanning.
    pub async fn scan_review_file(
        &self,
        worktree_path: &str,
        file: &ReviewFile,
        feedback: Option<&str>,
        previous_finding: Option<&str>,
    ) -> Result<Vec<ReviewFinding>> {
        let cwd = PathBuf::from(worktree_path);
        let model = self.config.ai.review.model.trim().to_string();
        if model.is_empty() {
            return Err(WisetreeError::other("ai.review model is not configured."));
        }
        let tables_path = materialize_review_tables().await;
        let prompt = build_review_scan_prompt(file, &tables_path, feedback, previous_finding);
        let mut run_args: Vec<String> = vec![
            "run".to_string(),
            prompt,
            "-m".to_string(),
            model,
            "--agent".to_string(),
            "plan".to_string(),
        ];
        run_args.extend(run_variant_args(&self.config.ai.review.thinking));
        let run_args_ref: Vec<&str> = run_args.iter().map(String::as_str).collect();
        let output = time::timeout(
            REVIEW_SCAN_TIMEOUT,
            run_command(&self.opencode_binary, &run_args_ref, Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("opencode review scan timed out after 240s"))?
        .map_err(WisetreeError::other)?;

        parse_review_findings(
            &output,
            &file.path,
            &file.commentable_lines,
            &file.annotated_diff,
        )
        .ok_or_else(|| WisetreeError::other("could not parse findings from the review AI output."))
    }

    /// Scan the WHOLE diff once for missing test coverage with a single
    /// captured opencode call. Coverage is deliberately owned by this one
    /// pass: whether an application change is tested depends on the test
    /// side of the diff (and the repo's existing tests), which no per-file
    /// scan can see — and letting each parallel scan judge it made them
    /// re-raise the same "add tests" recommendation as duplicate comments.
    pub async fn scan_review_coverage(
        &self,
        worktree_path: &str,
        files: &[ReviewFile],
    ) -> Result<Vec<ReviewFinding>> {
        let cwd = PathBuf::from(worktree_path);
        let model = self.config.ai.review.model.trim().to_string();
        if model.is_empty() {
            return Err(WisetreeError::other("ai.review model is not configured."));
        }
        let prompt = build_review_coverage_prompt(files);
        let mut run_args: Vec<String> = vec![
            "run".to_string(),
            prompt,
            "-m".to_string(),
            model,
            "--agent".to_string(),
            "plan".to_string(),
        ];
        run_args.extend(run_variant_args(&self.config.ai.review.thinking));
        let run_args_ref: Vec<&str> = run_args.iter().map(String::as_str).collect();
        let output = time::timeout(
            REVIEW_SCAN_TIMEOUT,
            run_command(&self.opencode_binary, &run_args_ref, Some(&cwd)),
        )
        .await
        .map_err(|_| WisetreeError::other("opencode coverage scan timed out after 240s"))?
        .map_err(WisetreeError::other)?;

        parse_coverage_findings(&output, files).ok_or_else(|| {
            WisetreeError::other("could not parse findings from the coverage AI output.")
        })
    }

    /// Post one approved finding to the PR — inline when it carries a
    /// validated line anchor, as a general PR comment otherwise. All
    /// deterministic; the body was previewed verbatim to the user.
    pub async fn post_review_finding(
        &self,
        worktree_path: &str,
        owner: &str,
        repo: &str,
        number: u64,
        head_sha: &str,
        finding: &ReviewFinding,
    ) -> Result<()> {
        let cwd = PathBuf::from(worktree_path);
        let body = finding.comment_body();
        let result = match finding.line {
            Some(line) => {
                let endpoint = format!("repos/{owner}/{repo}/pulls/{number}/comments");
                let mut args: Vec<String> = vec![
                    "api".to_string(),
                    endpoint,
                    "-f".to_string(),
                    format!("body={body}"),
                    "-f".to_string(),
                    format!("commit_id={head_sha}"),
                    "-f".to_string(),
                    format!("path={}", finding.file),
                    "-F".to_string(),
                    format!("line={line}"),
                    "-f".to_string(),
                    "side=RIGHT".to_string(),
                ];
                if let Some(start) = finding.start_line {
                    args.push("-F".to_string());
                    args.push(format!("start_line={start}"));
                    args.push("-f".to_string());
                    args.push("start_side=RIGHT".to_string());
                }
                let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
                time::timeout(
                    REVIEW_POST_TIMEOUT,
                    run_command(&self.gh_binary, &args_ref, Some(&cwd)),
                )
                .await
                .map_err(|_| WisetreeError::other("gh api comment timed out"))?
            }
            None => {
                let number_arg = number.to_string();
                time::timeout(
                    REVIEW_POST_TIMEOUT,
                    run_command(
                        &self.gh_binary,
                        &["pr", "comment", &number_arg, "--body", &body],
                        Some(&cwd),
                    ),
                )
                .await
                .map_err(|_| WisetreeError::other("gh pr comment timed out"))?
            }
        };
        result.map(|_| ()).map_err(WisetreeError::other)
    }

    /// Submit the review summary built from the posted findings, either as a
    /// blocking `--request-changes` review or a non-blocking `--comment` one.
    pub async fn submit_review_summary(
        &self,
        worktree_path: &str,
        number: u64,
        body: &str,
        request_changes: bool,
    ) -> Result<()> {
        let cwd = PathBuf::from(worktree_path);
        let number_arg = number.to_string();
        let mode = if request_changes {
            "--request-changes"
        } else {
            "--comment"
        };
        time::timeout(
            REVIEW_POST_TIMEOUT,
            run_command(
                &self.gh_binary,
                &["pr", "review", &number_arg, mode, "--body", body],
                Some(&cwd),
            ),
        )
        .await
        .map_err(|_| WisetreeError::other("gh pr review timed out"))?
        .map_err(WisetreeError::other)?;
        Ok(())
    }

    // ── "Bugkill" pipeline ─────────────────────────────────────────────
    //
    // AI is called in exactly three places: investigate (once, captured),
    // fix (per selected attempt, live PTY), and the judge micro-call behind
    // the "Other" button. Everything else — gates, snapshots, change-set
    // scans, commits, reverts, cleanup — is deterministic git work below.
    // No `gh`, no pushes anywhere in this pipeline.

    /// Deterministic Bugkill pre-flight: model + opencode gates, the
    /// clean-tree gate, the untracked baseline snapshot, base-ref
    /// resolution, and detection of a resumable `BUG_INVESTIGATION.md`.
    pub async fn bugkill_preflight(&self, worktree_path: &str) -> Result<BugkillPreflightOutcome> {
        if self.config.ai.bugkill.investigate.model.trim().is_empty() {
            return Ok(BugkillPreflightOutcome::AiNotConfigured);
        }
        if !binary_available(&self.opencode_binary) {
            return Ok(BugkillPreflightOutcome::AiUnavailable);
        }
        let cwd = PathBuf::from(worktree_path);
        let status = self.bugkill_git_status(&cwd).await?;

        let investigation = tokio::fs::read_to_string(cwd.join(INVESTIGATION_FILE))
            .await
            .ok()
            .map(|content| parse_investigation_md(&content));

        // Clean-tree gate: untracked files are allowed; tracked changes
        // block. With a parseable investigation file the changes are almost
        // certainly leftovers from an interrupted attempt (by I2 the tracked
        // tree was clean when it started) — offer to discard; otherwise the
        // user must commit or stash first.
        if !status.tracked.is_empty() {
            return Ok(match &investigation {
                Some(Some(_)) => BugkillPreflightOutcome::LeftoverAttempt {
                    tracked: status.tracked,
                },
                _ => BugkillPreflightOutcome::DirtyTree {
                    count: status.tracked.len(),
                },
            });
        }

        let untracked_snapshot = hash_untracked(&cwd, &status.untracked).await;
        let base_ref = resolve_base_ref_with_binary(&self.git_binary, &cwd, None).await;
        let resume = match investigation {
            None => BugkillResumeState::Absent,
            Some(None) => BugkillResumeState::Unparseable,
            Some(Some(investigation)) => {
                // Unverdicted-attempt recovery: a row committed but never
                // answered strands as implemented + no verdict. Recover its
                // commit sha so Resume can re-ask the Verdict question.
                let mut unverdicted = None;
                if let Some(row) = investigation
                    .hypotheses
                    .iter()
                    .find(|h| h.implemented && h.worked.is_none())
                {
                    unverdicted = Some(BugkillUnverdicted {
                        row_number: row.number,
                        sha: self.bugkill_recover_attempt_sha(&cwd, row.number).await,
                    });
                }
                BugkillResumeState::Parsed {
                    investigation,
                    unverdicted,
                }
            }
        };
        Ok(BugkillPreflightOutcome::Ready(Box::new(BugkillPreflight {
            untracked_snapshot,
            base_ref,
            resume,
        })))
    }

    /// Build the spawn parameters for the live investigation: the full
    /// opencode **TUI** pinned to the read-only Plan agent, embedded in the
    /// AI Activity panel so the user watches opencode's own rendering. The
    /// TUI never exits on its own — the App detects completion through an
    /// `OpencodeTurnWatcher` and reads the transcript from opencode's
    /// database. `corrective` appends the stricter-contract suffix used on
    /// the single retry after a parse failure.
    pub fn prepare_bugkill_investigate(
        &self,
        worktree_path: &str,
        bug_description: &str,
        base_ref: Option<&str>,
        corrective: bool,
    ) -> Result<FixApplyHandoff> {
        let slot = &self.config.ai.bugkill.investigate;
        let model = slot.model.trim().to_string();
        if model.is_empty() {
            return Err(WisetreeError::other(
                "ai.bugkill.investigate model is not configured.",
            ));
        }
        if !binary_available(&self.opencode_binary) {
            return Err(WisetreeError::other("opencode CLI is not on PATH."));
        }
        let cwd = PathBuf::from(worktree_path);
        let mut prompt = build_bug_investigate_prompt(bug_description, base_ref);
        if corrective {
            prompt = format!(
                "{prompt}\n\nYour previous output could not be parsed. Reply with ONLY the \
                 delimited block, exactly as specified."
            );
        }
        // The opencode TUI takes no `--variant`; it honors reasoning effort
        // solely via the persisted `model.json`, so seed it before spawning.
        seed_opencode_tui_variant(&model, &slot.thinking);
        let opencode_args: Vec<String> = vec![
            "--prompt".to_string(),
            prompt,
            "-m".to_string(),
            model,
            "--agent".to_string(),
            "plan".to_string(),
            cwd.to_string_lossy().to_string(),
        ];
        Ok(FixApplyHandoff {
            opencode_binary: self.opencode_binary.clone(),
            opencode_args,
            cwd,
        })
    }

    /// Build the spawn parameters for one live fix attempt. The fix AI
    /// receives the bug description plus exactly one hypothesis row
    /// (invariant I3) — never the table, never prior attempts.
    pub async fn prepare_bugkill_fix(
        &self,
        worktree_path: &str,
        bug_description: &str,
        row: &BugHypothesis,
        feedback: Option<&str>,
    ) -> Result<FixApplyHandoff> {
        let slot = &self.config.ai.bugkill.fix;
        let model = slot.model.trim().to_string();
        if model.is_empty() {
            return Err(WisetreeError::other(
                "ai.bugkill.fix model is not configured.",
            ));
        }
        if !binary_available(&self.opencode_binary) {
            return Err(WisetreeError::other("opencode CLI is not on PATH."));
        }
        let cwd = PathBuf::from(worktree_path);
        let prompt = build_bug_fix_prompt(bug_description, row, feedback);
        // The opencode TUI takes no `--variant`; it honors reasoning effort
        // solely via the persisted `model.json`, so seed it before spawning.
        seed_opencode_tui_variant(&model, &slot.thinking);
        let mut opencode_args: Vec<String> =
            vec!["--prompt".to_string(), prompt, "-m".to_string(), model];
        opencode_args.push(cwd.to_string_lossy().to_string());
        Ok(FixApplyHandoff {
            opencode_binary: self.opencode_binary.clone(),
            opencode_args,
            cwd,
        })
    }

    /// Classify the user's freeform "Other" answer as fixed / not fixed /
    /// unclear with one tiny captured call. A parse failure — or a failed
    /// call — is treated as `Unclear`, never as an error screen: the user
    /// still owes a Yes/No and loses nothing.
    pub async fn bugkill_judge(
        &self,
        worktree_path: &str,
        row: &BugHypothesis,
        user_text: &str,
    ) -> Result<BugkillVerdict> {
        let slot = &self.config.ai.bugkill.judge;
        let model = slot.model.trim().to_string();
        if model.is_empty() {
            return Err(WisetreeError::other(
                "ai.bugkill.judge model is not configured.",
            ));
        }
        let cwd = PathBuf::from(worktree_path);
        let prompt = build_bug_judge_prompt(row, user_text);
        let mut run_args: Vec<String> = vec![
            "run".to_string(),
            prompt,
            "-m".to_string(),
            model,
            "--agent".to_string(),
            "plan".to_string(),
        ];
        run_args.extend(run_variant_args(&slot.thinking));
        let run_args_ref: Vec<&str> = run_args.iter().map(String::as_str).collect();
        let transcript = time::timeout(
            BUGKILL_JUDGE_TIMEOUT,
            run_command(&self.opencode_binary, &run_args_ref, Some(&cwd)),
        )
        .await
        .unwrap_or_else(|_| Err("the judge call timed out".to_string()));
        Ok(match transcript {
            Ok(output) => parse_judge_verdict(&output).unwrap_or(BugkillVerdict {
                result: JudgeResult::Unclear,
                reason: String::new(),
            }),
            Err(_) => BugkillVerdict {
                result: JudgeResult::Unclear,
                reason: "The judge call failed — please answer Yes or No.".to_string(),
            },
        })
    }

    /// Fresh tracked/untracked snapshot of the worktree, with content hashes
    /// for the untracked files. Used right before an attempt (baseline) and
    /// right after opencode exits (change-set scan).
    pub async fn bugkill_snapshot(&self, worktree_path: &str) -> Result<BugkillSnapshot> {
        let cwd = PathBuf::from(worktree_path);
        let status = self.bugkill_git_status(&cwd).await?;
        let untracked = hash_untracked(&cwd, &status.untracked).await;
        Ok(BugkillSnapshot {
            tracked: status.tracked,
            untracked,
        })
    }

    /// Commit one applied attempt — the harness, never the AI. Stages each
    /// change-set path individually (never `git add -A`/`-u`; the modified
    /// pre-existing untracked files and `BUG_INVESTIGATION.md` are already
    /// excluded from `commit_paths`), then commits as
    /// `bugkill: attempt #N — <solution first line>`, or amends the existing
    /// attempt commit on a retry-with-feedback (safe: that commit is
    /// unpushed `HEAD`). Returns the (new) commit sha.
    pub async fn bugkill_commit_attempt(
        &self,
        worktree_path: &str,
        changes: &AttemptChanges,
        number: usize,
        solution: &str,
        amend: bool,
    ) -> Result<String> {
        let cwd = PathBuf::from(worktree_path);
        if changes.commit_paths.is_empty() {
            return Err(WisetreeError::other("nothing to commit for this attempt."));
        }
        for path in &changes.commit_paths {
            self.bugkill_git(&cwd, &["add", "--", path])
                .await
                .map_err(WisetreeError::other)?;
        }
        let subject = attempt_commit_subject(number, solution);
        let commit_args: Vec<&str> = if amend {
            vec!["commit", "--amend", "--no-edit"]
        } else {
            vec!["commit", "-m", &subject]
        };
        self.bugkill_git(&cwd, &commit_args).await.map_err(|err| {
            WisetreeError::other(if err.trim().is_empty() {
                "git commit failed after staging the attempt.".to_string()
            } else {
                err
            })
        })?;
        let sha = run_command(&self.git_binary, &["rev-parse", "HEAD"], Some(&cwd))
            .await
            .map_err(WisetreeError::other)?;
        Ok(sha.trim().to_string())
    }

    /// Discard *uncommitted* attempt changes: for each path, unstage it and
    /// either restore it from `HEAD` or delete it from disk when `HEAD`
    /// never had it. Used by the Esc-abort during `Fixing` and by the
    /// preflight leftover-attempt recovery. A *committed* attempt is always
    /// undone with [`Self::bugkill_rollback`] instead — never this.
    pub async fn bugkill_abort_cleanup(&self, worktree_path: &str, paths: &[String]) -> Result<()> {
        let cwd = PathBuf::from(worktree_path);
        for path in paths {
            // Unstage in case opencode (against instructions) staged it.
            let _ = self.bugkill_git(&cwd, &["reset", "-q", "--", path]).await;
            let head_spec = format!("HEAD:{path}");
            let in_head = time::timeout(
                BUGKILL_GIT_TIMEOUT,
                run_command(
                    &self.git_binary,
                    &["cat-file", "-e", &head_spec],
                    Some(&cwd),
                ),
            )
            .await
            .map(|result| result.is_ok())
            .unwrap_or(false);
            if in_head {
                self.bugkill_git(&cwd, &["checkout", "HEAD", "--", path])
                    .await
                    .map_err(WisetreeError::other)?;
            } else {
                let _ = tokio::fs::remove_file(cwd.join(path)).await;
            }
        }
        Ok(())
    }

    /// Run one git command inside `cwd` under the Bugkill timeout, returning
    /// [`run_command`]'s shape (`Ok(stdout)` / `Err(stderr)`) plus a
    /// `"git <sub> timed out"` error on timeout. Lock contention is recovered
    /// transparently inside `run_command` ([`retry_on_git_lock`]); the timeout
    /// bounds the whole recovery sequence.
    async fn bugkill_git(&self, cwd: &Path, args: &[&str]) -> std::result::Result<String, String> {
        let subcommand = args.first().copied().unwrap_or("command");
        time::timeout(
            BUGKILL_GIT_TIMEOUT,
            run_command(&self.git_binary, args, Some(cwd)),
        )
        .await
        .unwrap_or_else(|_| Err(format!("git {subcommand} timed out")))
    }

    /// History-preserving rollback of a committed attempt:
    /// `git revert --no-edit <sha>`. By invariant I2 the attempt commit is
    /// still `HEAD` when this runs, so the revert applies cleanly by
    /// construction. Lock contention is recovered transparently
    /// ([`Self::bugkill_git`]); any other non-zero exit is a hard error.
    pub async fn bugkill_rollback(&self, worktree_path: &str, sha: &str) -> Result<()> {
        let cwd = PathBuf::from(worktree_path);
        self.bugkill_git(&cwd, &["revert", "--no-edit", sha])
            .await
            .map_err(|err| {
                WisetreeError::other(format!("git revert --no-edit {sha} failed: {err}"))
            })?;
        Ok(())
    }

    async fn bugkill_git_status(
        &self,
        cwd: &Path,
    ) -> Result<crate::services::bugkill::PorcelainStatus> {
        // `--untracked-files=all` lists files inside untracked directories
        // individually, so the per-path snapshot/commit/cleanup always works
        // on real files, never on a `dir/` placeholder. `-z` keeps paths
        // unquoted (NUL-terminated records) so exotic filenames survive as
        // valid pathspecs — see `parse_porcelain_v2`.
        let output = time::timeout(
            BUGKILL_GIT_TIMEOUT,
            run_command(
                &self.git_binary,
                &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
                Some(cwd),
            ),
        )
        .await
        .map_err(|_| WisetreeError::other("git status timed out"))?
        .map_err(WisetreeError::other)?;
        Ok(parse_porcelain_v2(&output))
    }

    /// Newest commit whose subject starts `bugkill: attempt #N — `, for the
    /// unverdicted-attempt recovery. `None` when no such commit exists.
    async fn bugkill_recover_attempt_sha(&self, cwd: &Path, number: usize) -> Option<String> {
        let output = time::timeout(
            BUGKILL_GIT_TIMEOUT,
            run_command(
                &self.git_binary,
                &["log", "-n", "200", "--format=%H%x09%s"],
                Some(cwd),
            ),
        )
        .await
        .ok()?
        .ok()?;
        let prefix = attempt_commit_prefix(number);
        output.lines().find_map(|line| {
            let (sha, subject) = line.split_once('\t')?;
            subject.starts_with(&prefix).then(|| sha.trim().to_string())
        })
    }

    // ── "Develop" pipeline ─────────────────────────────────────────────
    //
    // AI is called in exactly two places: plan (once per revision, live TUI
    // watched by the turn watcher) and implement (one run per section on a
    // Ralph Loop, or a single run for the whole plan). Everything else —
    // gates, PLAN.md rendering/parsing, progress tracking, the approval
    // loop — is deterministic Rust. The AI never reads or writes PLAN.md.

    /// Deterministic Develop pre-flight: model + opencode gates, base-ref
    /// resolution, and detection of a resumable `PLAN.md`.
    pub async fn develop_preflight(&self, worktree_path: &str) -> Result<DevelopPreflightOutcome> {
        if self.config.ai.develop.plan.model.trim().is_empty()
            || self.config.ai.develop.implement.model.trim().is_empty()
        {
            return Ok(DevelopPreflightOutcome::AiNotConfigured);
        }
        if !binary_available(&self.opencode_binary) {
            return Ok(DevelopPreflightOutcome::AiUnavailable);
        }
        let cwd = PathBuf::from(worktree_path);
        let status = self
            .develop_git(&cwd, &["status", "--porcelain", "--untracked-files=all"])
            .await
            .map_err(WisetreeError::other)?;
        if !status.trim().is_empty() {
            return Err(WisetreeError::other(
                "Develop requires a clean worktree before starting.".to_string(),
            ));
        }
        let resume = match tokio::fs::read_to_string(cwd.join(PLAN_FILE)).await.ok() {
            None => DevelopResumeState::Absent,
            Some(content) => match parse_plan_md(&content) {
                None => DevelopResumeState::Unparseable,
                Some(plan) => DevelopResumeState::Parsed(plan),
            },
        };
        let base_ref = resolve_base_ref_with_binary(&self.git_binary, &cwd, None).await;
        Ok(DevelopPreflightOutcome::Ready(Box::new(DevelopPreflight {
            base_ref,
            resume,
        })))
    }

    /// Build the spawn parameters for one live planning run: the full
    /// opencode **TUI** pinned to the read-only Plan agent, embedded in the
    /// AI Activity panel. The TUI never exits on its own — the App detects
    /// completion through an `OpencodeTurnWatcher` and reads the transcript
    /// from opencode's database. `previous_plan` + `feedback` are set on a
    /// revision after the user rejects the plan; `corrective` appends the
    /// stricter-contract suffix used on the single retry after a parse
    /// failure.
    pub fn prepare_develop_plan(
        &self,
        worktree_path: &str,
        task_description: &str,
        base_ref: Option<&str>,
        previous_plan: Option<&str>,
        feedback: Option<&str>,
        corrective: bool,
    ) -> Result<FixApplyHandoff> {
        let slot = &self.config.ai.develop.plan;
        let model = slot.model.trim().to_string();
        if model.is_empty() {
            return Err(WisetreeError::other(
                "ai.develop.plan model is not configured.",
            ));
        }
        if !binary_available(&self.opencode_binary) {
            return Err(WisetreeError::other("opencode CLI is not on PATH."));
        }
        let cwd = PathBuf::from(worktree_path);
        let mut prompt =
            build_develop_plan_prompt(task_description, base_ref, previous_plan, feedback);
        if corrective {
            prompt = format!(
                "{prompt}\n\nYour previous output could not be parsed. Reply with ONLY the \
                 delimited blocks, exactly as specified."
            );
        }
        // The opencode TUI takes no `--variant`; it honors reasoning effort
        // solely via the persisted `model.json`, so seed it before spawning.
        seed_opencode_tui_variant(&model, &slot.thinking);
        let opencode_args: Vec<String> = vec![
            "--prompt".to_string(),
            prompt,
            "-m".to_string(),
            model,
            "--agent".to_string(),
            "plan".to_string(),
            cwd.to_string_lossy().to_string(),
        ];
        Ok(FixApplyHandoff {
            opencode_binary: self.opencode_binary.clone(),
            opencode_args,
            cwd,
        })
    }

    /// Build the spawn parameters for one live implement run. `sections`
    /// holds only the section block(s) this run must build — one section on
    /// a Ralph Loop, all pending sections otherwise — so no tokens are spent
    /// re-reading the rest of the plan; `outline` is the one-line-per-section
    /// roadmap (names + statuses, never bodies) that keeps the run in its
    /// lane.
    pub fn prepare_develop_implement(
        &self,
        worktree_path: &str,
        task_description: &str,
        sections: &str,
        outline: &str,
        check_failure: Option<&str>,
    ) -> Result<FixApplyHandoff> {
        let slot = &self.config.ai.develop.implement;
        let model = slot.model.trim().to_string();
        if model.is_empty() {
            return Err(WisetreeError::other(
                "ai.develop.implement model is not configured.",
            ));
        }
        if !binary_available(&self.opencode_binary) {
            return Err(WisetreeError::other("opencode CLI is not on PATH."));
        }
        let cwd = PathBuf::from(worktree_path);
        let prompt = build_develop_implement_prompt(
            task_description,
            sections,
            outline,
            self.config.develop.check_command.trim(),
            check_failure,
        );
        // The opencode TUI takes no `--variant`; it honors reasoning effort
        // solely via the persisted `model.json`, so seed it before spawning.
        seed_opencode_tui_variant(&model, &slot.thinking);
        let opencode_args: Vec<String> = vec![
            "--prompt".to_string(),
            prompt,
            "-m".to_string(),
            model,
            cwd.to_string_lossy().to_string(),
        ];
        Ok(FixApplyHandoff {
            opencode_binary: self.opencode_binary.clone(),
            opencode_args,
            cwd,
        })
    }

    /// Run the configured check command (Ralph-canon backpressure) in the
    /// worktree, deterministically — no AI. The command is run through the
    /// user's login shell so their PATH / toolchain shims resolve exactly as
    /// in a real terminal; stdout and stderr are merged (test runners split
    /// across both) and the tail is kept. A non-zero exit or a timeout is a
    /// `Failed`; the caller decides what to do with it (never the harness
    /// silently). Assumes a non-blank command — the pipeline skips the check
    /// entirely when `develop.checkCommand` is empty, so this is never called
    /// with nothing to run.
    pub async fn develop_run_check(&self, worktree_path: &str) -> DevelopCheckOutcome {
        let command = self.config.develop.check_command.trim().to_string();
        let cwd = PathBuf::from(worktree_path);
        let (shell, args) = login_shell_check_command(&command);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = time::timeout(
            DEVELOP_CHECK_TIMEOUT,
            run_command_combined(&shell, &arg_refs, Some(&cwd)),
        )
        .await;
        match result {
            Ok(Ok(())) => DevelopCheckOutcome::Passed,
            Ok(Err(output)) => DevelopCheckOutcome::Failed {
                output: clip_output_tail(&output, DEVELOP_CHECK_OUTPUT_MAX_BYTES),
            },
            Err(_) => DevelopCheckOutcome::Failed {
                output: format!("`{command}` timed out after 10 minutes."),
            },
        }
    }

    /// Commit one finished section — the harness, never the AI — as a
    /// Ralph-canon checkpoint. Stages everything **except** the harness-owned
    /// `PLAN.md` (`git add -A -- . ':(exclude)PLAN.md'`), then commits with
    /// `subject` (build it with [`develop_commit_subject`]). Returns the new
    /// sha, or `Ok(None)` when there was nothing to commit (the run made no
    /// change, or touched only `PLAN.md`) so the caller records no checkpoint
    /// rather than erroring.
    pub async fn develop_commit_section(
        &self,
        worktree_path: &str,
        subject: &str,
    ) -> Result<Option<String>> {
        let cwd = PathBuf::from(worktree_path);
        // Ensure a plan staged before this checkpoint cannot be committed.
        self.develop_git(&cwd, &["reset", "--", DEVELOP_PLAN_FILE])
            .await
            .map_err(WisetreeError::other)?;
        // Stage all tracked + untracked changes but the plan file. The
        // pathspec `:(exclude)PLAN.md` drops it wherever it sits.
        let exclude_plan = format!(":(exclude){DEVELOP_PLAN_FILE}");
        self.develop_git(&cwd, &["add", "-A", "--", ".", &exclude_plan])
            .await
            .map_err(WisetreeError::other)?;
        // Nothing staged (empty diff, or only PLAN.md changed) → no commit.
        let staged = self
            .develop_git(&cwd, &["diff", "--cached", "--name-only"])
            .await
            .map_err(WisetreeError::other)?;
        if staged.trim().is_empty() {
            return Ok(None);
        }
        self.develop_git(&cwd, &["commit", "-m", subject])
            .await
            .map_err(|err| {
                WisetreeError::other(if err.trim().is_empty() {
                    "git commit failed after staging the section.".to_string()
                } else {
                    err
                })
            })?;
        let sha = run_command(&self.git_binary, &["rev-parse", "HEAD"], Some(&cwd))
            .await
            .map_err(WisetreeError::other)?;
        Ok(Some(sha.trim().to_string()))
    }

    /// Run one git command inside `cwd` under the Develop check timeout,
    /// mirroring [`Self::bugkill_git`].
    async fn develop_git(&self, cwd: &Path, args: &[&str]) -> std::result::Result<String, String> {
        let subcommand = args.first().copied().unwrap_or("command");
        time::timeout(
            BUGKILL_GIT_TIMEOUT,
            run_command(&self.git_binary, args, Some(cwd)),
        )
        .await
        .unwrap_or_else(|_| Err(format!("git {subcommand} timed out")))
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
        self.apply_ai_status(&mut rows).await;

        rows.sort_by_key(|row| (!row.worktree.is_main, row.worktree.path.clone()));
        Ok(rows)
    }

    /// Run one global AI-status scan (off the async runtime) and apply the
    /// per-worktree report to every row. On timeout or panic we fall back to
    /// the last successful index so rows keep their previous values instead
    /// of flickering to `⬜ Pending` on every slow tick. The very first tick
    /// (no cached index yet) still degrades to empty so the column doesn't
    /// block the dashboard refresh.
    async fn apply_ai_status(&self, rows: &mut [DashboardRow]) {
        let svc = self.ai_status.clone();
        let scan = tokio::task::spawn_blocking(move || svc.build_index());
        let index: AiStatusIndex =
            match tokio::time::timeout(Duration::from_millis(AI_STATUS_BUDGET_MS), scan).await {
                Ok(Ok(fresh)) => {
                    if let Ok(mut cached) = self.last_ai_index.lock() {
                        *cached = Some(fresh.clone());
                    }
                    fresh
                }
                _ => self
                    .last_ai_index
                    .lock()
                    .ok()
                    .and_then(|cached| cached.clone())
                    .unwrap_or_default(),
            };
        for row in rows.iter_mut() {
            let report: AiStatusReport = self
                .ai_status
                .report_for(&index, Path::new(&row.worktree.path));
            row.ai_status = Some(report);
        }
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
            ai_status: None,
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

    /// Compute the commit-level ahead/behind of HEAD relative to the first
    /// reachable ref in `upstream/main`, `upstream/master`, `origin/main`,
    /// `origin/master`. `ahead`/`behind` come from
    /// `git rev-list --left-right --count` (matching `GitService::branch_status`),
    /// and `insertions`/`deletions` come from a follow-up
    /// `git diff --shortstat <upstream>` so the "Diff" column can render the
    /// line-level change set. Returns `None` when none of those remote refs are
    /// reachable.
    async fn fetch_upstream_diff(&self, cwd: &Path) -> Option<BranchStatus> {
        let upstream = resolve_base_ref_with_binary(&self.git_binary, cwd, None).await?;
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
                &["diff", "--shortstat", &upstream],
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

    /// Best-effort refresh of the PR base branch's remote-tracking ref so the
    /// dashboard's behind-count reflects commits another developer pushed to
    /// the base. Without this the count is measured against a possibly-stale
    /// `origin/main`, so a behind-but-conflict-free PR never surfaces the
    /// "Update" command in repos that don't enforce up-to-date branches.
    ///
    /// Fetches a single branch with an explicit destination refspec
    /// (`+<branch>:refs/remotes/<remote>/<branch>`) so only the one
    /// remote-tracking ref `fetch_upstream_diff` reads is updated — no
    /// `--all`, no extra branches, minimal transfer. Failures (offline, auth,
    /// missing ref) are swallowed: a stale tracking ref just means the count
    /// lags until the next successful fetch, never a stalled tick.
    ///
    /// Returns `true` only when the fetch actually advanced the base's
    /// remote-tracking ref, so the caller can re-render the refined
    /// behind-count exactly when there is new data — and skip the churn when
    /// the ref was already current.
    async fn fetch_base_ref(&self) -> bool {
        let Some(base_ref) =
            resolve_base_ref_with_binary(&self.git_binary, &self.git_root, None).await
        else {
            return false;
        };
        let Some((remote, branch)) = base_ref.split_once('/') else {
            return false;
        };
        let before = self.rev_parse(&base_ref).await;
        let refspec = format!("+{branch}:refs/remotes/{remote}/{branch}");
        let _ = time::timeout(
            BASE_FETCH_TIMEOUT,
            run_command(
                &self.git_binary,
                &["fetch", remote, &refspec],
                Some(&self.git_root),
            ),
        )
        .await;
        let after = self.rev_parse(&base_ref).await;
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

    /// Decide which branches need a PR refresh, then (if any) issue a single
    /// batched GraphQL request and update the cache.
    ///
    /// `on_cycle` is decided by the watch loop based on `next_pr_fetch_at`:
    /// `true` once per refresh period, `false` between periods. Off-cycle
    /// runs still pick up brand-new branches and SHA changes so the UI
    /// keeps up with local commits without disturbing the cycle rhythm.
    async fn refresh_pull_requests(&self, rows: &[DashboardRow], on_cycle: bool) -> bool {
        if !self.pr_enrichment_enabled() || self.is_rate_limited() {
            return false;
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
            return false;
        }

        let Some((owner, repo)) = self.resolve_repo_slug().await else {
            return false;
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
                if !to_fetch.is_empty() {
                    state.dirty = true;
                }
                // Successful round-trip — clear any prior rate-limit state.
                state.rate_limited_until = None;
                state.rate_limit_notice_sent = false;
                true
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
                false
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

/// `--variant <thinking>` for a non-interactive `opencode run` call (always
/// supported there). Empty thinking (the persisted "Default") yields no args.
fn run_variant_args(thinking: &str) -> Vec<String> {
    let thinking = thinking.trim();
    if thinking.is_empty() {
        Vec::new()
    } else {
        vec!["--variant".to_string(), thinking.to_string()]
    }
}

/// opencode's sentinel value for "no reasoning override" in `model.json`. The
/// TUI persists this (not the empty string) when a model is cycled back to no
/// variant, and treats it as "no override" because it's never a real variant
/// name (which are reasoning efforts like `high`/`max`).
const OPENCODE_NO_VARIANT: &str = "default";

/// Persist the configured reasoning effort for `model` into opencode's
/// `model.json` so the **TUI** opens at that thinking strength.
///
/// opencode's interactive TUI (`opencode [project]`) exposes no `--variant`
/// flag — through at least 1.17.x it resolves a model's reasoning effort
/// *solely* from its persisted state file (the "saved preference" the user
/// otherwise cycles with ctrl+t), keyed by `provider/model`. Only `opencode
/// run` takes `--variant` (see [`run_variant_args`]). So to launch a TUI flow
/// at the user's configured strength we seed that exact entry here first.
///
/// Best-effort: any IO/JSON error is swallowed so a read-only or absent state
/// dir never blocks the AI flow — it just launches without the seeded effort,
/// exactly as before this seeding existed.
fn seed_opencode_tui_variant(model: &str, thinking: &str) {
    seed_opencode_tui_variant_at(
        &crate::constants::opencode_model_state_file(),
        model,
        thinking,
    );
}

/// [`seed_opencode_tui_variant`] against an explicit state-file path, so tests
/// can target a tempdir instead of the developer's real `$XDG_STATE_HOME`.
fn seed_opencode_tui_variant_at(path: &Path, model: &str, thinking: &str) {
    let model = model.trim();
    if model.is_empty() {
        return;
    }
    let current = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let next = merged_variant_state(current, model, thinking);
    let _ = write_json_atomic(path, &next);
}

/// Pure merge: take the current `model.json` value (or `None`) and return it
/// with `variant[model]` set to the resolved effort, preserving every other
/// field (`recent`, `favorite`, other models' variants). An empty `thinking`
/// (the persisted "Default") writes [`OPENCODE_NO_VARIANT`], which clears any
/// stale effort a prior session left for this model. Factored out so the merge
/// is unit-testable without touching the filesystem.
fn merged_variant_state(
    current: Option<serde_json::Value>,
    model: &str,
    thinking: &str,
) -> serde_json::Value {
    let effort = {
        let t = thinking.trim();
        if t.is_empty() {
            OPENCODE_NO_VARIANT
        } else {
            t
        }
    };

    let mut root = current
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    // Safe: `root` is guaranteed to be an object by the filter/fallback above.
    let obj = root.as_object_mut().expect("root is a json object");
    let variant = obj
        .entry("variant")
        .or_insert_with(|| serde_json::json!({}));
    if !variant.is_object() {
        *variant = serde_json::json!({});
    }
    variant
        .as_object_mut()
        .expect("variant is a json object")
        .insert(
            model.to_string(),
            serde_json::Value::String(effort.to_string()),
        );
    root
}

/// Atomically write `value` as JSON to `path` — write a sibling temp file then
/// rename over the target — so a concurrent opencode reader never observes a
/// half-written file (mirrors opencode's own `writeJsonAtomic`). Parent dirs
/// are created as needed.
fn write_json_atomic(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // pid-suffixed sibling so two wisetree processes can't collide on the temp.
    let tmp = path.with_file_name(format!(
        ".{}.wisetree.{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "model.json".to_string()),
        std::process::id()
    ));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn is_rate_limit_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("rate limit") || lower.contains("rate-limit")
}

fn pr_refresh_period(_config: &DashboardConfig) -> Duration {
    Duration::from_millis(PR_REFRESH_PERIOD_MS)
}

fn wise_merge_candidate(row: &DashboardRow) -> Option<WiseMergeCandidate> {
    if row.worktree.is_main {
        return None;
    }
    let pr = row.pull_request.as_ref()?;
    if !matches!(pr.state, PrState::Open) {
        return None;
    }
    if !matches!(pr.checks_status, Some(CheckStatus::Passed)) {
        return None;
    }
    if !matches!(pr.merge_status, Some(MergeStatus::Clean)) {
        return None;
    }
    if !matches!(pr.review_status, None | Some(ReviewStatus::Approved)) {
        return None;
    }
    let base_ref_name = pr
        .base_ref_name
        .as_ref()
        .filter(|value| !value.is_empty())?;
    let base_repository = pr
        .base_repository
        .as_ref()
        .filter(|value| !value.is_empty())?;
    let head_ref_oid = pr.head_ref_oid.as_ref().filter(|value| !value.is_empty())?;

    Some(WiseMergeCandidate {
        number: pr.number,
        worktree_path: row.worktree.path.clone(),
        base_ref_name: base_ref_name.clone(),
        base_repository: base_repository.clone(),
        head_ref_oid: head_ref_oid.clone(),
    })
}

fn validate_wise_merge_base(candidate: &WiseMergeCandidate, base_ref: &str) -> Result<()> {
    let expected_base = branch_name_from_ref(base_ref);
    if candidate.base_ref_name == expected_base {
        Ok(())
    } else {
        Err(WisetreeError::other(format!(
            "PR base `{}` does not match resolved base ref `{base_ref}`.",
            candidate.base_ref_name
        )))
    }
}

fn validate_wise_merge_repository(candidate: &WiseMergeCandidate, base_repo: &str) -> Result<()> {
    if candidate.base_repository == base_repo {
        Ok(())
    } else {
        Err(WisetreeError::other(format!(
            "PR base repository `{}` does not match resolved base repository `{base_repo}`.",
            candidate.base_repository
        )))
    }
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

/// Resolve the `(owner, repo)` of the repository a PR was opened against from
/// the JSON `gh pr view <N> --json url` returns. The base repo is read off the
/// canonical `url` (`https://github.com/<owner>/<repo>/pull/<N>`), which
/// `parse_github_slug` already handles. `gh` resolves that repo from the local
/// remotes, so this is correct even when the branch lives on a fork and the PR
/// targets an upstream repo — unlike the `headRepository*` fields, which point
/// at the (possibly fork) head.
fn parse_pr_repo_json(body: &str) -> Option<(String, String)> {
    #[derive(Deserialize)]
    struct PrUrlJson {
        #[serde(default)]
        url: String,
    }
    let parsed: PrUrlJson = serde_json::from_str(body).ok()?;
    parse_github_slug(&parsed.url)
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
            "b{i}: pullRequests(headRefName: \"{}\", states: [OPEN, CLOSED, MERGED], first: 1, orderBy: {{field: CREATED_AT, direction: DESC}}) {{ nodes {{ number url title state isDraft baseRefName baseRepository {{ nameWithOwner }} headRefOid mergeStateStatus reviewDecision labels(first: 20) {{ nodes {{ name }} }} reviewRequests(first: 100) {{ totalCount nodes {{ requestedReviewer {{ __typename ... on User {{ login }} }} }} }} latestOpinionatedReviews(first: 100) {{ nodes {{ state author {{ login }} }} }} latestReviews(first: 100) {{ nodes {{ state author {{ login }} }} }} commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state contexts(first: 100) {{ nodes {{ __typename ... on CheckRun {{ status conclusion }} ... on StatusContext {{ state }} }} }} }} }} }} }} }} }} ",
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
                    labels,
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

/// `gh <args>`, retrying a couple of times when it fails with a transient
/// GitHub-side 5xx ([`is_transient_gh_error`]) — anything else (404, auth,
/// …) returns on the first try, same as [`run_command`].
async fn run_gh_command(
    binary: &Path,
    args: &[&str],
    cwd: Option<&Path>,
) -> std::result::Result<String, String> {
    let mut attempt = 0;
    loop {
        match run_command(binary, args, cwd).await {
            Err(err) if attempt < GH_TRANSIENT_RETRIES && is_transient_gh_error(&err) => {
                attempt += 1;
                time::sleep(GH_TRANSIENT_RETRY_DELAY).await;
            }
            result => return result,
        }
    }
}

fn is_transient_gh_error(err: &str) -> bool {
    ["HTTP 500", "HTTP 502", "HTTP 503", "HTTP 504"]
        .iter()
        .any(|code| err.contains(code))
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

/// Run a command and stream each line of stdout/stderr through `activity` as
/// it arrives. Returns stdout on success or stderr on failure, same as
/// `run_command`. When `activity` is `None` the function falls back to the
/// non-streaming `run_command` path.
async fn run_command_streamed(
    binary: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    activity: Option<&mpsc::UnboundedSender<(String, ActivityKind)>>,
) -> std::result::Result<String, String> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let Some(tx) = activity else {
        return run_command(binary, args, cwd).await;
    };

    let mut cmd = Command::new(binary);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }

    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let mut out_lines = BufReader::new(stdout).lines();
    let mut err_lines = BufReader::new(stderr).lines();
    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let mut out_done = false;
    let mut err_done = false;

    while !out_done || !err_done {
        tokio::select! {
            line = out_lines.next_line(), if !out_done => {
                match line {
                    Ok(Some(l)) => {
                        let clean = strip_ansi(&l);
                        stdout_buf.push_str(&clean);
                        stdout_buf.push('\n');
                        if !clean.is_empty() {
                            let _ = tx.send((clean, ActivityKind::Stdout));
                        }
                    }
                    _ => out_done = true,
                }
            }
            line = err_lines.next_line(), if !err_done => {
                match line {
                    Ok(Some(l)) => {
                        let clean = strip_ansi(&l);
                        stderr_buf.push_str(&clean);
                        stderr_buf.push('\n');
                        if !clean.is_empty() {
                            let _ = tx.send((clean, ActivityKind::Stderr));
                        }
                    }
                    _ => err_done = true,
                }
            }
        }
    }

    let status = child.wait().await.map_err(|e| e.to_string())?;
    if status.success() {
        Ok(stdout_buf.trim().to_string())
    } else {
        Err(stderr_buf.trim().to_string())
    }
}

fn send_ai_activity(
    progress: Option<&mpsc::UnboundedSender<UpdateProgress>>,
    event: AiActivityEvent,
) {
    if let Some(tx) = progress {
        let _ = tx.send(UpdateProgress::AiOutput(event));
    }
}

/// Resolve the base ref for a worktree — the branch it was cut from, which
/// is what every PR command diffs and merges against. Preference order:
///
/// 1. `pr_base_ref` — GitHub's own `baseRefName` for an existing PR, mapped
///    onto a reachable remote-tracking ref. Authoritative when a PR exists.
/// 2. The branch's tracked upstream (`@{upstream}`), which `git worktree add`
///    seeds from the source branch (e.g. `upstream/release-0.41`). Skipped
///    once the branch has been pushed with `-u` and now tracks its own
///    `origin/<branch>` counterpart — that is never a base.
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
    // 1. Prefer GitHub's known base branch for the PR, mapped to whichever
    //    remote-tracking ref we actually have locally.
    if let Some(name) = pr_base_ref.map(str::trim).filter(|n| !n.is_empty()) {
        for remote in ["upstream", "origin"] {
            let candidate = format!("{remote}/{name}");
            if ref_is_reachable(git_binary, cwd, &candidate).await {
                return Some(candidate);
            }
        }
    }

    // 2. Trust the branch's own tracked upstream, unless it is the branch's
    //    own pushed ref (`origin/<self>`), which a PR is never based on.
    if let Some(upstream) = tracked_upstream(git_binary, cwd).await {
        let is_own_push_ref = remote_name_from_ref(&upstream) == Some("origin")
            && current_branch_name(git_binary, cwd)
                .await
                .is_some_and(|branch| branch_name_from_ref(&upstream) == branch);
        if !is_own_push_ref && ref_is_reachable(git_binary, cwd, &upstream).await {
            return Some(upstream);
        }
    }

    // 3. Fall back to the conventional priority list.
    for candidate in BASE_REF_PRIORITY {
        if ref_is_reachable(git_binary, cwd, candidate).await {
            return Some(candidate.to_string());
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
/// List the worktree's tracked, uncommitted changes — every path that
/// differs from `HEAD` in the index or the working tree (modifications,
/// staged additions, deletions, renames). `git diff --name-only HEAD`
/// emits clean paths with no status prefix and, by design, omits untracked
/// files: those rarely block a merge, and on the rare collision git's own
/// "untracked working tree files would be overwritten" message still
/// surfaces through the normal `MergeFailed` path. Used by `update_branch`
/// as a pre-flight guard so a dirty tree fails fast with an actionable
/// message rather than a raw `git merge` refusal.
async fn dirty_tracked_files(git_binary: &Path, cwd: &Path) -> Vec<String> {
    match run_command(git_binary, &["diff", "--name-only", "HEAD"], Some(cwd)).await {
        Ok(out) => out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

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

/// The task prompt fed to opencode when resolving merge conflicts. The body
/// of `prompts/merger.md` is embedded at compile time so the binary has no
/// runtime dependency on the prompt file.
/// `MERGE_REF` is substituted with the resolved base ref; `CONFLICTED_FILES`
/// is substituted with the bulleted list of unmerged paths produced by
/// `git diff --name-only --diff-filter=U`.
///
/// The prompt deliberately ships **without** YAML frontmatter so it stays a
/// plain instruction body no matter which CLI harness executes it.
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

/// Fallback PR template used when the repo ships none of the well-known
/// template files. Mirrors the `filler` skill's reference template so the
/// native flow produces the same section layout the team is used to.
const ENRICH_TEMPLATE_FALLBACK: &str = "# Description ✍️

Brief explanation of the PR purpose


# Overview 🔍

Overview of the feature if possible (screenshot, gif, etc)


# Test Guidance 🦮

Step-by-step process to test the changes related to this Pull Request


# Ticket 🎫

[{{ACRONYM}}-{{NUMBER}}](link/to/{{ACRONYM}}-{{NUMBER}})
";

/// Extract a ticket id from a branch name. Matches the `filler`/`creator`
/// skills: acronym `DIGIT` or `DPMS` (case-insensitive), an optional
/// hyphen, then digits — normalized to uppercase `ACRONYM-NUM`.
fn extract_ticket(branch: &str) -> Option<String> {
    static TICKET: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(DIGIT|DPMS)-?(\d+)").unwrap());
    let caps = TICKET.captures(branch)?;
    let acronym = caps.get(1)?.as_str().to_uppercase();
    let number = caps.get(2)?.as_str();
    Some(format!("{acronym}-{number}"))
}

/// Read the repository's PR template, trying the well-known locations in
/// order and falling back to the embedded default. Searched relative to the
/// worktree so each checkout's `.github/` is honored.
async fn read_pr_template(cwd: &Path) -> String {
    const CANDIDATES: [&str; 7] = [
        ".github/pull_request_template.md",
        ".github/PULL_REQUEST_TEMPLATE.md",
        ".github/PULL_REQUEST_TEMPLATE/pull_request_template.md",
        "docs/pull_request_template.md",
        "docs/PULL_REQUEST_TEMPLATE.md",
        "pull_request_template.md",
        "PULL_REQUEST_TEMPLATE.md",
    ];
    for rel in CANDIDATES {
        if let Ok(content) = tokio::fs::read_to_string(cwd.join(rel)).await {
            if !content.trim().is_empty() {
                return content;
            }
        }
    }
    ENRICH_TEMPLATE_FALLBACK.to_string()
}

/// Render `prompts/enricher.md` into a concrete prompt by substituting the
/// harness-gathered inputs. The diff is embedded last (and truncated if
/// huge) so the earlier placeholder substitutions never scan the diff body.
fn build_enrich_prompt(
    base_ref: &str,
    branch: &str,
    ticket: &str,
    git_log: &str,
    git_diff: &str,
    template: &str,
) -> String {
    const ENRICHER_PROMPT: &str = include_str!("../../prompts/enricher.md");
    let diff = truncate_for_prompt(git_diff, ENRICH_DIFF_MAX_BYTES);
    ENRICHER_PROMPT
        .replace("BASE_REF", base_ref)
        .replace("CURRENT_BRANCH", branch)
        .replace("TICKET", ticket)
        .replace("PR_TEMPLATE", template)
        .replace("GIT_LOG", git_log)
        .replace("GIT_DIFF", &diff)
}

// ── "Fix Pull Request" helpers (deterministic, unit-tested) ────────────

/// Build the GraphQL query for the Fix loop. It returns, in one round-trip:
/// every inline review thread (with the `isResolved` / `isOutdated` flags we
/// filter on, plus each comment's `isMinimized` and `viewerDidAuthor` flags),
/// and every PR-level review summary body (the text left when submitting a
/// review, not anchored to a line). Both feed `parse_and_group_review_feedback`.
fn build_fix_feedback_query(owner: &str, repo: &str, number: u64) -> String {
    format!(
        "query {{ repository(owner: \"{}\", name: \"{}\") {{ pullRequest(number: {}) {{ \
         reviewThreads(first: 100) {{ nodes {{ isResolved \
         comments(first: 50) {{ nodes {{ databaseId path line originalLine isMinimized \
         viewerDidAuthor body author {{ login }} }} }} }} }} \
         reviews(first: 100) {{ nodes {{ state body author {{ login }} }} }} }} }} }}",
        escape_graphql_string(owner),
        escape_graphql_string(repo),
        number
    )
}

/// Parse the review-feedback GraphQL response and group the survivors.
///
/// Inline threads: resolved threads and minimized comments are dropped. A
/// thread is also dropped when our own resolution reply ("Addressed in …" or
/// the no-change reply) is its *most recent* comment — a previous run handled
/// it and nobody has objected since. If the reviewer replied *after* that
/// resolution reply (e.g. "you changed the wrong function"), the thread is
/// pending again and kept, so the planner re-reads the whole discussion —
/// including our prior reply — and tries again. Outdated threads with no reply
/// from us are likewise kept (anchored via `originalLine`).
/// Surviving inline comments are grouped by (file, line) in first-seen order.
///
/// PR-level review summaries (review bodies not anchored to a line) cannot be
/// replied to in-thread, so every submitted, non-empty one is folded into a
/// single group (file / line / reply id all `None`) appended last. The planning
/// AI judges them together and any reply goes back as one general PR comment.
fn parse_and_group_review_feedback(body: &str) -> std::result::Result<Vec<CommentGroup>, String> {
    #[derive(Deserialize)]
    struct Resp {
        data: Option<RespData>,
        errors: Option<Vec<GhErr>>,
    }
    #[derive(Deserialize)]
    struct RespData {
        repository: Option<Repo>,
    }
    #[derive(Deserialize)]
    struct Repo {
        #[serde(rename = "pullRequest")]
        pull_request: Option<Pr>,
    }
    #[derive(Deserialize)]
    struct Pr {
        #[serde(rename = "reviewThreads")]
        review_threads: Conn<Thread>,
        reviews: Option<Conn<Review>>,
    }
    #[derive(Deserialize)]
    struct Conn<T> {
        #[serde(default = "Vec::new")]
        nodes: Vec<T>,
    }
    #[derive(Deserialize)]
    struct Thread {
        #[serde(rename = "isResolved", default)]
        is_resolved: bool,
        comments: Conn<RawComment>,
    }
    #[derive(Deserialize)]
    struct RawComment {
        #[serde(rename = "databaseId")]
        database_id: Option<u64>,
        path: Option<String>,
        line: Option<u64>,
        #[serde(rename = "originalLine")]
        original_line: Option<u64>,
        #[serde(rename = "isMinimized", default)]
        is_minimized: bool,
        #[serde(rename = "viewerDidAuthor", default)]
        viewer_did_author: bool,
        #[serde(default)]
        body: String,
        author: Option<Author>,
    }
    /// A submitted PR-level review. `body` is the summary text; `state` is one
    /// of APPROVED / CHANGES_REQUESTED / COMMENTED / DISMISSED / PENDING.
    #[derive(Deserialize)]
    struct Review {
        #[serde(default)]
        state: String,
        #[serde(default)]
        body: String,
        author: Option<Author>,
    }
    #[derive(Deserialize)]
    struct Author {
        #[serde(default)]
        login: String,
    }
    #[derive(Deserialize)]
    struct GhErr {
        #[serde(default)]
        message: String,
    }

    let resp: Resp = serde_json::from_str(body).map_err(|e| format!("invalid gh response: {e}"))?;
    if let Some(errors) = resp.errors {
        let joined = errors
            .into_iter()
            .map(|e| e.message)
            .filter(|m| !m.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        if !joined.is_empty() {
            return Err(joined);
        }
    }
    let pr = resp
        .data
        .and_then(|d| d.repository)
        .and_then(|r| r.pull_request)
        .ok_or_else(|| "missing pull request in response".to_string())?;

    let login = |a: &Option<Author>| -> String {
        a.as_ref()
            .map(|a| a.login.clone())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "reviewer".to_string())
    };

    let mut groups: Vec<CommentGroup> = Vec::new();
    let mut index: HashMap<(String, u64), usize> = HashMap::new();

    for thread in pr.review_threads.nodes {
        if thread.is_resolved {
            continue;
        }
        let surviving: Vec<RawComment> = thread
            .comments
            .nodes
            .into_iter()
            .filter(|c| !c.is_minimized)
            .collect();
        let Some(first) = surviving.first() else {
            continue;
        };
        // A thread is handled only when our own resolution reply is its *last*
        // word — nobody, not even the reviewer, responded after it. If the
        // reviewer followed up ("you changed the wrong function"), the thread is
        // pending again and must be re-analysed with the whole discussion. This
        // replaces the old `isOutdated`-based skip, which dropped any outdated
        // thread we'd ever replied to and so swallowed reviewer follow-ups.
        if surviving
            .last()
            .is_some_and(|c| c.viewer_did_author && is_resolution_reply(&c.body))
        {
            continue;
        }
        let file = first.path.clone();
        let line = first.line.or(first.original_line);
        let reply_id = first.database_id;
        let mapped: Vec<ReviewComment> = surviving
            .iter()
            .filter(|c| !c.body.trim().is_empty())
            .map(|c| ReviewComment {
                author: login(&c.author),
                body: c.body.clone(),
                database_id: c.database_id,
                viewer_did_author: c.viewer_did_author,
            })
            .collect();
        if mapped.is_empty() {
            continue;
        }
        // Merge threads sharing the same file + line into one group.
        if let (Some(f), Some(l)) = (&file, line) {
            if let Some(&gi) = index.get(&(f.clone(), l)) {
                groups[gi].comments.extend(mapped);
                continue;
            }
            index.insert((f.clone(), l), groups.len());
        }
        groups.push(CommentGroup {
            file,
            line,
            reply_comment_id: reply_id,
            comments: mapped,
        });
    }

    // Fold every PR-level review summary body into one trailing group. These
    // are not line-anchored and cannot be replied to in-thread, so the whole
    // set shares a single group (file / line / reply id `None`, so the reply
    // falls back to a general PR comment) and is judged together by one
    // planning call. PENDING reviews aren't submitted yet and DISMISSED ones
    // were explicitly retracted, so both are excluded along with empty bodies.
    let mut summaries: Vec<ReviewComment> = Vec::new();
    for review in pr.reviews.map(|c| c.nodes).unwrap_or_default() {
        let state = review.state.to_ascii_uppercase();
        if state == "PENDING" || state == "DISMISSED" || review.body.trim().is_empty() {
            continue;
        }
        summaries.push(ReviewComment {
            author: login(&review.author),
            body: review.body,
            // PR-level summaries have no inline-comment id to react to.
            database_id: None,
            viewer_did_author: false,
        });
    }
    if !summaries.is_empty() {
        groups.push(CommentGroup {
            file: None,
            line: None,
            reply_comment_id: None,
            comments: summaries,
        });
    }

    Ok(groups)
}

/// Read a generous window of the targeted file around `line`, numbered so the
/// AI can cite exact lines. Whole-file (capped) when not line-anchored.
async fn read_code_window(cwd: &Path, file: &str, line: Option<u64>) -> String {
    let Ok(content) = tokio::fs::read_to_string(cwd.join(file)).await else {
        return String::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let (start, end) = match line {
        Some(l) if l >= 1 => {
            let l0 = (l as usize).saturating_sub(1);
            let start = l0.saturating_sub(FIX_CODE_WINDOW_RADIUS);
            let end = (l0 + FIX_CODE_WINDOW_RADIUS + 1).min(total);
            (start, end)
        }
        _ => (0, total.min(FIX_CODE_WINDOW_RADIUS * 2 + 1)),
    };
    let mut out = String::new();
    for (offset, text) in lines[start..end].iter().enumerate() {
        let n = start + offset + 1;
        out.push_str(&format!("{n:>5} | {text}\n"));
        if out.len() > FIX_CODE_MAX_BYTES {
            out.push_str("...(truncated)\n");
            break;
        }
    }
    out
}

/// Render `prompts/fixer_plan.md` into a concrete planning prompt. The big
/// user-controlled blocks (comments, code, processed history) are substituted
/// last so an earlier placeholder can't be clobbered by a value containing a
/// later token. `history` carries the comments + replies + fixes already
/// handled earlier in this same run, so a later comment that refers back to
/// them ("good job, but we have misalignments") is judged with that context
/// instead of in isolation.
fn build_fix_plan_prompt(
    group: &CommentGroup,
    code: &str,
    feedback: Option<&str>,
    previous_plan: Option<&str>,
    history: Option<&str>,
) -> String {
    const PLAN_PROMPT: &str = include_str!("../../prompts/fixer_plan.md");
    let file = group.file.clone().unwrap_or_default();
    let lines = group.line.map(|l| l.to_string()).unwrap_or_default();
    let code = if code.trim().is_empty() {
        "(no code context — PR-level comment)".to_string()
    } else {
        code.to_string()
    };
    let history = match history {
        Some(h) if !h.trim().is_empty() => h,
        _ => "(none — this is the first comment of the run)",
    };
    PLAN_PROMPT
        .replace("FILE_PATH", &file)
        .replace("COMMENT_LINES", &lines)
        .replace("USER_FEEDBACK", feedback.unwrap_or("(none)"))
        .replace("PREVIOUS_PLAN", previous_plan.unwrap_or("(none)"))
        .replace("REVIEW_COMMENTS", &group.combined_text())
        .replace("CODE_CONTEXT", &code)
        .replace("PROCESSED_HISTORY", history)
}

/// Render `prompts/fixer_apply.md` for the live apply phase.
fn build_fix_apply_prompt(group: &CommentGroup, plan: &FixPlan) -> String {
    const APPLY_PROMPT: &str = include_str!("../../prompts/fixer_apply.md");
    let files = group
        .file
        .clone()
        .unwrap_or_else(|| "(see the comment — no single target file)".to_string());
    let plan_text = format!(
        "{}\n\nValidity: {}\n\nExplanation: {}\n\nChange:\n{}",
        plan.summary.trim(),
        plan.validity.trim(),
        plan.explanation.trim(),
        plan.change.trim()
    );
    APPLY_PROMPT
        .replace("TARGET_FILES", &files)
        .replace("REVIEW_COMMENT", &group.combined_text())
        .replace("APPROVED_PLAN", &plan_text)
}

/// Truncate a Bugkill prompt input to `max_bytes` on a char boundary,
/// appending the truncation marker. Applied before templating so the
/// rendered prompt argv never approaches OS limits.
fn truncate_bugkill_field(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &text[..end])
}

/// Render `prompts/bug_investigate.md` for the one-shot investigation call.
fn build_bug_investigate_prompt(bug_description: &str, base_ref: Option<&str>) -> String {
    const PROMPT: &str = include_str!("../../prompts/bug_investigate.md");
    PROMPT
        .replace(
            "BUG_DESCRIPTION",
            &truncate_bugkill_field(bug_description, BUGKILL_DESCRIPTION_MAX_BYTES),
        )
        .replace("BASE_REF", base_ref.unwrap_or("(none resolved)"))
}

/// Render `prompts/bug_fix.md` for one live fix attempt. Exactly one
/// hypothesis row goes in (invariant I3); `feedback` is only present on a
/// retry after the judge inferred `NOT_FIXED` from an "Other" answer.
fn build_bug_fix_prompt(
    bug_description: &str,
    row: &BugHypothesis,
    feedback: Option<&str>,
) -> String {
    const PROMPT: &str = include_str!("../../prompts/bug_fix.md");
    PROMPT
        .replace(
            "BUG_DESCRIPTION",
            &truncate_bugkill_field(bug_description, BUGKILL_DESCRIPTION_MAX_BYTES),
        )
        .replace(
            "CAUSE_DESCRIPTION",
            &truncate_bugkill_field(&row.description, BUGKILL_FIELD_MAX_BYTES),
        )
        .replace(
            "SOLUTION",
            &truncate_bugkill_field(&row.solution, BUGKILL_FIELD_MAX_BYTES),
        )
        .replace(
            "USER_FEEDBACK",
            &truncate_bugkill_field(feedback.unwrap_or(""), BUGKILL_FIELD_MAX_BYTES),
        )
}

/// Render `prompts/bug_judge.md` for the "Other"-answer micro-call.
fn build_bug_judge_prompt(row: &BugHypothesis, user_text: &str) -> String {
    const PROMPT: &str = include_str!("../../prompts/bug_judge.md");
    PROMPT
        .replace(
            "CAUSE_DESCRIPTION",
            &truncate_bugkill_field(&row.description, BUGKILL_FIELD_MAX_BYTES),
        )
        .replace(
            "SOLUTION",
            &truncate_bugkill_field(&row.solution, BUGKILL_FIELD_MAX_BYTES),
        )
        .replace(
            "USER_FEEDBACK",
            &truncate_bugkill_field(user_text, BUGKILL_FIELD_MAX_BYTES),
        )
}

/// Render `prompts/develop_plan.md` for one live planning run.
/// `previous_plan` is the compact contract-format rendering of the plan
/// being revised; both revision slots are empty on a first run.
fn build_develop_plan_prompt(
    task_description: &str,
    base_ref: Option<&str>,
    previous_plan: Option<&str>,
    feedback: Option<&str>,
) -> String {
    const PROMPT: &str = include_str!("../../prompts/develop_plan.md");
    PROMPT
        .replace(
            "TASK_DESCRIPTION",
            &truncate_bugkill_field(task_description, DEVELOP_TASK_MAX_BYTES),
        )
        .replace("BASE_REF", base_ref.unwrap_or("(none resolved)"))
        .replace(
            "PREVIOUS_PLAN",
            &truncate_bugkill_field(previous_plan.unwrap_or(""), DEVELOP_PLAN_MAX_BYTES),
        )
        .replace(
            "USER_FEEDBACK",
            &truncate_bugkill_field(feedback.unwrap_or(""), DEVELOP_FEEDBACK_MAX_BYTES),
        )
}

/// Render `prompts/develop_implement.md` for one live implement run. Only
/// the section block(s) the run must build go in — never the whole plan
/// file (token-efficiency invariant) — plus the compact outline so the run
/// knows what belongs to other sections. `check_command` is the check the
/// harness runs after the run (blank = none); `check_failure` is the
/// captured output from the previous run's failed check on a corrective
/// retry (empty on a first attempt).
fn build_develop_implement_prompt(
    task_description: &str,
    sections: &str,
    outline: &str,
    check_command: &str,
    check_failure: Option<&str>,
) -> String {
    const PROMPT: &str = include_str!("../../prompts/develop_implement.md");
    let check_command = if check_command.is_empty() {
        "(no automated check configured — rely on the acceptance criteria)".to_string()
    } else {
        check_command.to_string()
    };
    PROMPT
        .replace(
            "TASK_DESCRIPTION",
            &truncate_bugkill_field(task_description, DEVELOP_TASK_MAX_BYTES),
        )
        // `PLAN_OUTLINE` before `SECTIONS`: the outline placeholder contains
        // the word SECTIONS nowhere, but replacing the more specific token
        // first keeps the templating order-independent regardless.
        .replace(
            "PLAN_OUTLINE",
            &truncate_bugkill_field(outline, DEVELOP_FEEDBACK_MAX_BYTES),
        )
        .replace(
            "CHECK_COMMAND",
            &truncate_bugkill_field(&check_command, DEVELOP_FEEDBACK_MAX_BYTES),
        )
        .replace(
            "CHECK_FAILURE",
            &truncate_bugkill_field(check_failure.unwrap_or(""), DEVELOP_PLAN_MAX_BYTES),
        )
        .replace(
            "SECTIONS",
            &truncate_bugkill_field(sections, DEVELOP_PLAN_MAX_BYTES),
        )
}

/// Build a section-commit subject. Ralph mode passes the section's
/// `Some((number, name))` → `develop: section N — <name>`; single-run mode
/// passes `None` → `develop: implement plan`. The name's first line is
/// clipped to 60 chars so the subject stays one tidy line.
pub fn develop_commit_subject(section: Option<(usize, &str)>) -> String {
    match section {
        Some((number, name)) => {
            let first = name.lines().next().unwrap_or("").trim();
            let clipped: String = if first.chars().count() <= 60 {
                first.to_string()
            } else {
                let head: String = first.chars().take(60).collect();
                format!("{head}…")
            };
            format!("develop: section {number} — {clipped}")
        }
        None => "develop: implement plan".to_string(),
    }
}

/// Keep the last `max_bytes` of `text` on a char boundary, prefixing `…`
/// when clipped. Used for the check-command output tail (failures cluster at
/// the end of a test run).
fn clip_output_tail(text: &str, max_bytes: usize) -> String {
    let trimmed = text.trim_end();
    if trimmed.len() <= max_bytes {
        return trimmed.to_string();
    }
    let mut start = trimmed.len() - max_bytes;
    while start < trimmed.len() && !trimmed.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &trimmed[start..])
}

/// Build the `(shell, args)` that run `command` through the user's login
/// shell so their PATH / toolchain shims resolve as in a real terminal.
/// Mirrors `files::service`'s post-create runner: `-l` for shells that
/// support it, then `-c <command>`. No `-i` — there is no PTY.
fn login_shell_check_command(command: &str) -> (PathBuf, Vec<String>) {
    let shell = std::env::var("SHELL").ok();
    login_shell_check_command_for_shell(command, shell.as_deref())
}

fn login_shell_check_command_for_shell(
    command: &str,
    configured_shell: Option<&str>,
) -> (PathBuf, Vec<String>) {
    let shell = configured_shell
        .filter(|s| !s.is_empty())
        .unwrap_or("/bin/sh");
    let shell_name = Path::new(&shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("sh")
        .trim_start_matches('-');
    let mut args = Vec::new();
    if matches!(
        shell_name,
        "bash" | "zsh" | "fish" | "ksh" | "ksh93" | "mksh" | "tcsh" | "csh"
    ) {
        args.push("-l".to_string());
    }
    args.push("-c".to_string());
    args.push(command.to_string());
    (PathBuf::from(shell), args)
}

async fn read_bounded_tail<R>(mut reader: R, max_bytes: usize) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut tail = Vec::new();
    let mut chunk = [0_u8; 8192];

    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }

        if read >= max_bytes {
            tail.clear();
            tail.extend_from_slice(&chunk[read - max_bytes..read]);
        } else {
            let excess = tail.len().saturating_add(read).saturating_sub(max_bytes);
            if excess > 0 {
                tail.drain(..excess);
            }
            tail.extend_from_slice(&chunk[..read]);
        }
    }

    Ok(tail)
}

/// Like [`run_command_once`] but merges stdout+stderr and returns `Ok(())`
/// on a zero exit or `Err(combined_output)` otherwise — the shape the check
/// runner needs (a test runner writes results to both streams, and success
/// carries no message).
async fn run_command_combined(
    binary: &Path,
    args: &[&str],
    cwd: Option<&Path>,
) -> std::result::Result<(), String> {
    let mut cmd = Command::new(binary);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn().map_err(|err| err.to_string())?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let (status, stdout, stderr) = tokio::try_join!(
        child.wait(),
        read_bounded_tail(stdout, DEVELOP_CHECK_OUTPUT_MAX_BYTES),
        read_bounded_tail(stderr, DEVELOP_CHECK_OUTPUT_MAX_BYTES),
    )
    .map_err(|err| err.to_string())?;

    if status.success() {
        Ok(())
    } else {
        let mut combined = stdout;
        combined.extend_from_slice(&stderr);
        let start = combined
            .len()
            .saturating_sub(DEVELOP_CHECK_OUTPUT_MAX_BYTES);
        Err(String::from_utf8_lossy(&combined[start..]).into_owned())
    }
}

/// Hash every untracked file (excluding `BUG_INVESTIGATION.md`) so a later
/// scan can tell attempt-created files from pre-existing ones and detect
/// modifications to the latter. Unreadable files hash as an empty string —
/// equal before and after, so they never show up as attempt changes.
async fn hash_untracked(cwd: &Path, untracked: &[String]) -> Vec<(String, String)> {
    let mut snapshot = Vec::new();
    for path in untracked {
        if path == INVESTIGATION_FILE {
            continue;
        }
        let hash = match tokio::fs::read(cwd.join(path)).await {
            Ok(bytes) => blake3::hash(&bytes).to_hex().to_string(),
            Err(_) => String::new(),
        };
        snapshot.push((path.clone(), hash));
    }
    snapshot
}

/// Parse the single machine-readable verdict block the planning AI emits.
/// Tolerant of surrounding transcript noise: locates the BEGIN/END markers
/// anywhere in `output`. Returns `None` when no valid block is present.
fn parse_fix_verdict(output: &str) -> Option<FixVerdict> {
    const BEGIN: &str = "===WISETREE-FIX-BEGIN===";
    const END: &str = "===WISETREE-FIX-END===";
    let after_begin = &output[output.find(BEGIN)? + BEGIN.len()..];
    let block = &after_begin[..after_begin.find(END)?];

    let verdict = block
        .lines()
        .find_map(|l| l.trim().strip_prefix("VERDICT:"))?
        .trim()
        .to_lowercase();

    match verdict.as_str() {
        "praise" => Some(FixVerdict::Praise),
        "reply" => {
            let reply = extract_fix_section(block, "REPLY")?.trim().to_string();
            (!reply.is_empty()).then_some(FixVerdict::Reply(reply))
        }
        "fix" => {
            let summary = extract_fix_section(block, "SUMMARY")
                .unwrap_or_default()
                .trim()
                .to_string();
            let validity = extract_fix_section(block, "VALIDITY")
                .unwrap_or_default()
                .trim()
                .to_string();
            let explanation = extract_fix_section(block, "EXPLANATION")
                .unwrap_or_default()
                .trim()
                .to_string();
            let change = extract_fix_section(block, "CHANGE")
                .unwrap_or_default()
                .trim()
                .to_string();
            if summary.is_empty() && explanation.is_empty() && change.is_empty() {
                return None;
            }
            Some(FixVerdict::Fix(FixPlan {
                summary,
                validity,
                explanation,
                change,
            }))
        }
        _ => None,
    }
}

/// Extract the body of a `---NAME---` section: every line after the header up
/// to the next `---SECTION---` header (or the block end). A bare `---` markdown
/// rule does not terminate a section (the closing `---` must wrap a name).
fn extract_fix_section(block: &str, name: &str) -> Option<String> {
    let header = format!("---{name}---");
    let lines: Vec<&str> = block.lines().collect();
    let pos = lines.iter().position(|l| l.trim() == header)?;
    let mut body = Vec::new();
    for line in &lines[pos + 1..] {
        let t = line.trim();
        if t.len() > 6 && t.starts_with("---") && t.ends_with("---") {
            break;
        }
        body.push(*line);
    }
    Some(body.join("\n"))
}

/// Parse `gh pr view --json url,headRefOid,baseRefName` output into
/// `(owner, repo, head_sha, base_ref_name)`. The slug comes off the URL —
/// like [`parse_pr_repo_json`] — so fork-opened PRs resolve to their base
/// repo. `base_ref_name` is GitHub's target branch, used to reproduce the PR
/// diff locally when the API diff endpoint is unavailable.
fn parse_review_pr_json(body: &str) -> Option<(String, String, String, String)> {
    #[derive(Deserialize)]
    struct ReviewPrJson {
        #[serde(default)]
        url: String,
        #[serde(default, rename = "headRefOid")]
        head_ref_oid: String,
        #[serde(default, rename = "baseRefName")]
        base_ref_name: String,
    }
    let parsed: ReviewPrJson = serde_json::from_str(body).ok()?;
    if parsed.head_ref_oid.trim().is_empty() {
        return None;
    }
    let (owner, repo) = parse_github_slug(&parsed.url)?;
    Some((owner, repo, parsed.head_ref_oid, parsed.base_ref_name))
}

/// Split a unified diff (`gh pr diff` output) into per-file [`ReviewFile`]s.
/// Every line present in the new version of a file gets its new-side line
/// number inlined (removed lines stay unnumbered), and those numbers double
/// as the commentable-lines set the AI's anchors are validated against.
/// Binary and deleted files are skipped — there is nothing to comment on.
pub(crate) fn parse_review_diff(diff: &str) -> Vec<ReviewFile> {
    let mut files: Vec<ReviewFile> = Vec::new();
    let mut path: Option<String> = None;
    let mut annotated = String::new();
    let mut commentable: BTreeSet<u64> = BTreeSet::new();
    let mut new_line: u64 = 0;
    let mut in_hunk = false;
    let mut skip = false;

    fn flush(
        files: &mut Vec<ReviewFile>,
        path: &mut Option<String>,
        annotated: &mut String,
        commentable: &mut BTreeSet<u64>,
        skip: bool,
    ) {
        if let Some(p) = path.take() {
            if !skip && !commentable.is_empty() {
                files.push(ReviewFile {
                    path: p,
                    annotated_diff: std::mem::take(annotated).trim_end().to_string(),
                    commentable_lines: std::mem::take(commentable),
                    existing_comments: String::new(),
                    existing_keys: Vec::new(),
                });
            }
        }
        annotated.clear();
        commentable.clear();
    }

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            flush(
                &mut files,
                &mut path,
                &mut annotated,
                &mut commentable,
                skip,
            );
            in_hunk = false;
            skip = false;
            continue;
        }
        if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            skip = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            if rest.trim() == "/dev/null" {
                // Deleted file — no new side to comment on.
                skip = true;
            } else {
                path = Some(
                    rest.trim()
                        .strip_prefix("b/")
                        .unwrap_or(rest.trim())
                        .to_string(),
                );
            }
            continue;
        }
        if line.starts_with("--- ") {
            continue;
        }
        if line.starts_with("@@") {
            // `@@ -a,b +c,d @@` — the new side starts at line c.
            let start = line
                .split_whitespace()
                .find(|token| token.starts_with('+'))
                .and_then(|token| token[1..].split(',').next())
                .and_then(|c| c.parse::<u64>().ok());
            if let Some(start) = start {
                new_line = start;
                in_hunk = true;
                annotated.push_str(line);
                annotated.push('\n');
            }
            continue;
        }
        if !in_hunk || skip {
            continue;
        }
        match line.chars().next() {
            Some('+') => {
                annotated.push_str(&format!("{new_line:>6} {line}\n"));
                commentable.insert(new_line);
                new_line += 1;
            }
            Some('-') => {
                annotated.push_str(&format!("       {line}\n"));
            }
            Some('\\') => {
                // "\ No newline at end of file" — metadata, not content.
                annotated.push_str(&format!("       {line}\n"));
            }
            _ => {
                // Context line — part of the new side, commentable on RIGHT.
                annotated.push_str(&format!("{new_line:>6} {line}\n"));
                commentable.insert(new_line);
                new_line += 1;
            }
        }
    }
    flush(
        &mut files,
        &mut path,
        &mut annotated,
        &mut commentable,
        skip,
    );
    files
}

/// Per-file context extracted from the PR's existing inline comments: the
/// rendered block the scan prompt shows the AI, plus the structured keys of
/// the wisetree-format ones that back the deterministic duplicate filter.
#[derive(Debug, Default, Clone)]
pub(crate) struct ExistingComments {
    pub(crate) rendered: String,
    pub(crate) keys: Vec<ExistingFindingKey>,
}

/// Group the PR's existing inline review comments per file path, rendered as
/// `@author (line N): body` blocks. Input is the REST
/// `pulls/{n}/comments` JSON array; anything unparsable yields no context
/// (the scan just runs without dedup awareness).
pub(crate) fn parse_existing_review_comments(json: &str) -> HashMap<String, ExistingComments> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return HashMap::new();
    };
    let Some(items) = value.as_array() else {
        return HashMap::new();
    };
    let mut rendered: HashMap<String, Vec<String>> = HashMap::new();
    let mut keys: HashMap<String, Vec<ExistingFindingKey>> = HashMap::new();
    for item in items {
        let Some(path) = item.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        let body = item.get("body").and_then(|v| v.as_str()).unwrap_or("");
        if body.trim().is_empty() {
            continue;
        }
        let author = item
            .pointer("/user/login")
            .and_then(|v| v.as_str())
            .unwrap_or("reviewer");
        let line = item
            .get("line")
            .and_then(|v| v.as_u64())
            .or_else(|| item.get("original_line").and_then(|v| v.as_u64()));
        let anchor = line.map(|l| format!(" (line {l})")).unwrap_or_default();
        rendered
            .entry(path.to_string())
            .or_default()
            .push(format!("@{author}{anchor}: {}", body.trim()));
        if let Some(title) = wisetree_finding_title(body) {
            keys.entry(path.to_string())
                .or_default()
                .push(ExistingFindingKey { line, title });
        }
    }
    rendered
        .into_iter()
        .map(|(path, comments)| {
            let keys = keys.remove(&path).unwrap_or_default();
            (
                path,
                ExistingComments {
                    rendered: comments.join("\n\n"),
                    keys,
                },
            )
        })
        .collect()
}

/// The normalized title of a wisetree-format review comment, or `None` for a
/// human comment. Current comments lead with a `### {title}` heading and close
/// with the centered severity/category badge ([`ReviewFinding::comment_body`]);
/// the badge is the signature that tells them apart from an arbitrary human
/// heading. Older comments led with a `**[Cat] [Sev]**: {title}` header — still
/// recognized so dedup keeps working against PRs that carry them. Only the
/// leading lines are inspected so a quoted header deep in a human reply can't
/// produce a key.
fn wisetree_finding_title(body: &str) -> Option<String> {
    // Current format: the centered badge marks it as ours; the title is the
    // leading `### ` heading.
    if body.contains("<p align=\"center\">") {
        for line in body.lines().take(3) {
            if let Some(title) = line.trim().strip_prefix("### ") {
                let title = title.trim();
                if !title.is_empty() {
                    return Some(title.to_lowercase());
                }
            }
        }
    }
    // Legacy format: `**[Cat] [Sev]**: title`.
    for line in body.lines().take(3) {
        if let Some(rest) = line.trim().strip_prefix("**[") {
            if let Some((_, title)) = rest.split_once("]**: ") {
                let title = title.trim();
                if !title.is_empty() {
                    return Some(title.to_lowercase());
                }
            }
        }
    }
    None
}

/// #5 of the token-saving plan: drop findings the PR already carries as a
/// wisetree comment (same line anchor + same normalized title) instead of
/// trusting the model to honor the EXISTING_COMMENTS instruction — the
/// least reliable link on a re-run. Returns `(fresh, duplicates)`; the
/// duplicates surface as "Already posted" rows on the final report, never
/// silently.
pub fn split_duplicate_findings(
    findings: Vec<ReviewFinding>,
    existing: &[ExistingFindingKey],
) -> (Vec<ReviewFinding>, Vec<ReviewFinding>) {
    findings.into_iter().partition(|finding| {
        let title = finding.title.trim().to_lowercase();
        !existing
            .iter()
            .any(|key| key.line == finding.line && key.title == title)
    })
}

/// Collapse findings that duplicate one another *within a single run*, which
/// the per-file parallel scans can produce with different wording for the
/// same underlying issue. Two findings in the same file collapse when they
/// propose the same fix — identical [`normalize_suggestion`] text — or land
/// on the same line, since either way one comment already covers it. Callers
/// pass the findings already ordered by priority (severity, then diff order),
/// so the first occurrence — the one kept — is the highest-severity one and
/// the rest are returned as `duplicates`, surfaced as muted report rows
/// rather than dropped silently.
pub fn split_run_duplicate_findings(
    findings: Vec<ReviewFinding>,
) -> (Vec<ReviewFinding>, Vec<ReviewFinding>) {
    let mut seen_fixes: HashSet<(String, String)> = HashSet::new();
    let mut seen_anchors: HashSet<(String, u64)> = HashSet::new();
    let mut kept = Vec::with_capacity(findings.len());
    let mut duplicates = Vec::new();
    for finding in findings {
        let fix = normalize_suggestion(finding.suggestion.as_deref());
        let fix_key = (!fix.is_empty()).then(|| (finding.file.clone(), fix));
        let anchor_key = finding.line.map(|line| (finding.file.clone(), line));

        let is_duplicate = fix_key.as_ref().is_some_and(|k| seen_fixes.contains(k))
            || anchor_key
                .as_ref()
                .is_some_and(|k| seen_anchors.contains(k));
        if is_duplicate {
            duplicates.push(finding);
            continue;
        }
        if let Some(k) = fix_key {
            seen_fixes.insert(k);
        }
        if let Some(k) = anchor_key {
            seen_anchors.insert(k);
        }
        kept.push(finding);
    }
    (kept, duplicates)
}

/// Whitespace-insensitive form of a suggestion body, so two findings that
/// propose the same fix modulo indentation/blank lines compare equal. An
/// absent or blank suggestion normalizes to the empty string (no fix key).
fn normalize_suggestion(suggestion: Option<&str>) -> String {
    suggestion
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Write the curated reference tables next to the temp dir so the scan AI
/// can read them on demand instead of paying their token cost on every call.
/// Best-effort: on a write failure the prompt just points at "(unavailable)"
/// and the AI reviews from its compact checklists alone.
async fn materialize_review_tables() -> String {
    const TABLES: &str = include_str!("../../prompts/reviewer_tables.md");
    let path = std::env::temp_dir().join("wisetree-reviewer-tables.md");
    if tokio::fs::write(&path, TABLES).await.is_err() {
        return "(unavailable)".to_string();
    }
    path.to_string_lossy().to_string()
}

/// Deterministic pre-filter: files nobody reviews by hand are excluded
/// before any AI call — each skipped file would otherwise cost a full
/// scan (prompt + diff) for feedback no author wants. Path-based only, so
/// the decision is reproducible and visible on the final report.
pub(crate) fn review_skip_reason(path: &str) -> Option<&'static str> {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    let mut parts = lower.rsplit('/');
    let name = parts.next().unwrap_or_default();
    if parts.any(|dir| matches!(dir, "vendor" | "vendors" | "node_modules" | "third_party")) {
        return Some("vendored code");
    }
    if lower.split('/').any(|dir| dir == "__snapshots__") || name.ends_with(".snap") {
        return Some("test snapshot");
    }
    const LOCKFILES: [&str; 16] = [
        "cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lock",
        "bun.lockb",
        "go.sum",
        "gemfile.lock",
        "composer.lock",
        "poetry.lock",
        "uv.lock",
        "pipfile.lock",
        "podfile.lock",
        "mix.lock",
        "flake.lock",
        "packages.lock.json",
    ];
    if LOCKFILES.contains(&name) || name.ends_with(".lockfile") {
        return Some("lockfile");
    }
    if name.ends_with(".min.js") || name.ends_with(".min.css") || name.ends_with(".min.mjs") {
        return Some("minified asset");
    }
    if name.ends_with(".map") {
        return Some("source map");
    }
    if name.contains(".generated.") || name.ends_with(".pb.go") || name.ends_with("_pb2.py") {
        return Some("generated code");
    }
    None
}

/// Split the parsed diff into the files worth an AI scan and the ones the
/// deterministic filter excludes (with their reasons).
pub(crate) fn partition_reviewable_files(
    files: Vec<ReviewFile>,
) -> (Vec<ReviewFile>, Vec<ReviewSkippedFile>) {
    let mut reviewable = Vec::new();
    let mut skipped = Vec::new();
    for file in files {
        match review_skip_reason(&file.path) {
            Some(reason) => skipped.push(ReviewSkippedFile {
                path: file.path,
                reason,
            }),
            None => reviewable.push(file),
        }
    }
    (reviewable, skipped)
}

/// Classify a changed file as a test file so its scan uses the dedicated
/// test-quality prompt profile instead of the source one. Path-based
/// heuristic covering the common layouts: `tests`/`spec`/`__tests__`
/// directories, `test_*` / `*_test.*` / `*.test.*` / `*_spec.*` / `*.spec.*`
/// file names, and `conftest.py`.
pub(crate) fn is_test_file(path: &str) -> bool {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    let mut parts = lower.rsplit('/');
    let name = parts.next().unwrap_or_default();
    if parts.any(|dir| matches!(dir, "test" | "tests" | "spec" | "specs" | "__tests__")) {
        return true;
    }
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    stem == "conftest"
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with(".test")
        || stem.ends_with("_spec")
        || stem.ends_with(".spec")
}

/// Render the per-file scan (or revision) prompt: `prompts/reviewer_tester.md`
/// for test files (test-quality lens), `prompts/reviewer.md` for everything
/// else. Fixed tokens are substituted first and the user-controlled blocks
/// last so an earlier placeholder can't be clobbered by a value containing a
/// later token. `feedback` / `previous_finding` are only present on an
/// "Other" revision of a single finding.
fn build_review_scan_prompt(
    file: &ReviewFile,
    tables_path: &str,
    feedback: Option<&str>,
    previous_finding: Option<&str>,
) -> String {
    const SOURCE_PROMPT: &str = include_str!("../../prompts/reviewer.md");
    const TESTER_PROMPT: &str = include_str!("../../prompts/reviewer_tester.md");
    let template = if is_test_file(&file.path) {
        TESTER_PROMPT
    } else {
        SOURCE_PROMPT
    };
    let existing = if file.existing_comments.trim().is_empty() {
        "(none)".to_string()
    } else {
        truncate_for_prompt(&file.existing_comments, REVIEW_COMMENTS_MAX_BYTES)
    };
    template
        .replace("TABLES_PATH", tables_path)
        .replace("FILE_PATH", &file.path)
        .replace("USER_FEEDBACK", feedback.unwrap_or("(none)"))
        .replace("PREVIOUS_FINDING", previous_finding.unwrap_or("(none)"))
        .replace("EXISTING_COMMENTS", &existing)
        .replace(
            "FILE_DIFF",
            &truncate_for_prompt(&file.annotated_diff, REVIEW_DIFF_MAX_BYTES),
        )
}

/// Render the whole-diff coverage prompt (`prompts/reviewer_coverage.md`):
/// one `### FILE:` section per changed file, application code and tests
/// alike, so the single coverage pass sees everything the per-file scans see
/// combined — coverage is a cross-file judgment no per-file scan can make.
/// Substitution order mirrors [`build_review_scan_prompt`]: the (largest,
/// user-controlled) diff goes last.
fn build_review_coverage_prompt(files: &[ReviewFile]) -> String {
    const COVERAGE_PROMPT: &str = include_str!("../../prompts/reviewer_coverage.md");
    let mut diff = String::new();
    for file in files {
        diff.push_str(&format!("### FILE: {}\n", file.path));
        diff.push_str(&truncate_for_prompt(
            &file.annotated_diff,
            REVIEW_DIFF_MAX_BYTES,
        ));
        diff.push_str("\n\n");
    }
    let mut comments = String::new();
    for file in files {
        if !file.existing_comments.trim().is_empty() {
            comments.push_str(&format!(
                "### FILE: {}\n{}\n\n",
                file.path, file.existing_comments
            ));
        }
    }
    let existing = if comments.trim().is_empty() {
        "(none)".to_string()
    } else {
        truncate_for_prompt(&comments, REVIEW_COMMENTS_MAX_BYTES)
    };
    COVERAGE_PROMPT
        .replace("EXISTING_COMMENTS", &existing)
        .replace(
            "FULL_DIFF",
            &truncate_for_prompt(&diff, REVIEW_COVERAGE_DIFF_MAX_BYTES),
        )
}

/// Parse the coverage pass's findings block. Same block format as
/// [`parse_review_findings`] plus a `FILE:` header per finding, since the one
/// coverage call spans every changed file. The named file must be one of the
/// scanned [`ReviewFile`]s — a finding pointing anywhere else can't be posted
/// on the PR and is dropped. Line anchors are validated against that file's
/// commentable lines, and the category is pinned to Test Quality whatever the
/// model wrote.
pub(crate) fn parse_coverage_findings(
    output: &str,
    files: &[ReviewFile],
) -> Option<Vec<ReviewFinding>> {
    const BEGIN: &str = "===WISETREE-REVIEW-BEGIN===";
    const END: &str = "===WISETREE-REVIEW-END===";
    let after_begin = &output[output.find(BEGIN)? + BEGIN.len()..];
    let block = &after_begin[..after_begin.find(END)?];

    let mut findings = Vec::new();
    for chunk in block.split("---FINDING---").skip(1) {
        let chunk = chunk.split("---END-FINDING---").next().unwrap_or(chunk);
        let Some(file) = coverage_chunk_file(chunk, files) else {
            continue;
        };
        if let Some(mut finding) = parse_review_finding_chunk(
            chunk,
            &file.path,
            &file.commentable_lines,
            &file.annotated_diff,
        ) {
            finding.category = normalize_review_category("Test Quality");
            findings.push(finding);
        }
    }
    Some(findings)
}

/// Resolve one coverage chunk's `FILE:` header against the scanned files,
/// tolerating a `./` prefix. Read with the same header-zone rule as the other
/// fields: only above the first `---…---` section marker, so an explanation
/// quoting `FILE:` never leaks in.
fn coverage_chunk_file<'a>(chunk: &str, files: &'a [ReviewFile]) -> Option<&'a ReviewFile> {
    for line in chunk.lines() {
        let trimmed = line.trim();
        if trimmed.len() > 6 && trimmed.starts_with("---") && trimmed.ends_with("---") {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("FILE:") {
            let path = rest.trim().trim_start_matches("./");
            return files.iter().find(|f| f.path == path);
        }
    }
    None
}

/// Parse the single machine-readable findings block the scan AI emits.
/// Tolerant of surrounding transcript noise: locates the BEGIN/END markers
/// anywhere in `output`. Returns `None` when no block is present (the model
/// disobeyed the contract); `Some(vec![])` — a clean scan — when the block
/// carries no findings. Every line anchor is validated against
/// `commentable`; an invalid anchor downgrades the finding to file-level
/// rather than letting GitHub reject the comment. Suggestions that
/// reproduce the diff's current lines verbatim (checked against
/// `annotated_diff`) are stripped as no-ops.
pub(crate) fn parse_review_findings(
    output: &str,
    file: &str,
    commentable: &BTreeSet<u64>,
    annotated_diff: &str,
) -> Option<Vec<ReviewFinding>> {
    const BEGIN: &str = "===WISETREE-REVIEW-BEGIN===";
    const END: &str = "===WISETREE-REVIEW-END===";
    let after_begin = &output[output.find(BEGIN)? + BEGIN.len()..];
    let block = &after_begin[..after_begin.find(END)?];

    let mut findings = Vec::new();
    for chunk in block.split("---FINDING---").skip(1) {
        let chunk = chunk.split("---END-FINDING---").next().unwrap_or(chunk);
        if let Some(finding) = parse_review_finding_chunk(chunk, file, commentable, annotated_diff)
        {
            findings.push(finding);
        }
    }
    Some(findings)
}

/// Parse one `---FINDING---` chunk. Header fields (`CATEGORY:` …) are read
/// only above the first `---SECTION---` marker so an explanation that quotes
/// a `LINE:` never leaks into the header. A chunk with neither title nor
/// explanation is model noise, not a finding.
fn parse_review_finding_chunk(
    chunk: &str,
    file: &str,
    commentable: &BTreeSet<u64>,
    annotated_diff: &str,
) -> Option<ReviewFinding> {
    let header = |name: &str| -> Option<String> {
        let prefix = format!("{name}:");
        for line in chunk.lines() {
            let trimmed = line.trim();
            if trimmed.len() > 6 && trimmed.starts_with("---") && trimmed.ends_with("---") {
                break;
            }
            if let Some(rest) = trimmed.strip_prefix(&prefix) {
                return Some(rest.trim().to_string());
            }
        }
        None
    };

    let explanation = extract_fix_section(chunk, "EXPLANATION")
        .unwrap_or_default()
        .trim()
        .to_string();
    let suggestion = extract_fix_section(chunk, "SUGGESTION")
        .map(|s| s.trim_matches('\n').to_string())
        .filter(|s| !s.trim().is_empty());
    let mut title = header("TITLE").unwrap_or_default();
    if title.is_empty() {
        title = explanation
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| clip(l.trim(), 80))
            .unwrap_or_default();
    }
    if title.is_empty() && explanation.is_empty() {
        return None;
    }

    let line = header("LINE")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| commentable.contains(n));
    let start_line = match (
        header("START_LINE").and_then(|v| v.parse::<u64>().ok()),
        line,
    ) {
        (Some(start), Some(end)) if start < end && commentable.contains(&start) => Some(start),
        _ => None,
    };

    // #6 of the token-saving plan: a SUGGESTION that reproduces the current
    // lines verbatim is a no-op the author could "apply" for nothing — a
    // known model failure. Strip the block and keep the finding as prose.
    let suggestion = suggestion.filter(|body| {
        let Some(end) = line else {
            return true; // file-level: no lines to compare against
        };
        match diff_lines_text(annotated_diff, start_line.unwrap_or(end), end) {
            Some(current) => !code_lines_equal(body, &current),
            None => true,
        }
    });

    Some(ReviewFinding {
        category: normalize_review_category(&header("CATEGORY").unwrap_or_default()),
        severity: ReviewSeverity::parse(&header("SEVERITY").unwrap_or_default()),
        file: file.to_string(),
        start_line,
        line,
        title,
        explanation,
        suggestion,
    })
}

/// The current text of the new-side lines `start..=end`, recovered from the
/// annotated diff: each numbered line is `{n:>6} {marker}{content}` where
/// the marker is the diff's `+`/space. `None` when any line of the range is
/// absent from the diff (then no no-op check is possible).
fn diff_lines_text(annotated_diff: &str, start: u64, end: u64) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for n in start..=end {
        let prefix = format!("{n:>6} ");
        let numbered = annotated_diff
            .lines()
            .find_map(|l| l.strip_prefix(prefix.as_str()))?;
        let mut chars = numbered.chars();
        chars.next(); // drop the +/space diff marker
        out.push(chars.as_str());
    }
    Some(out.join("\n"))
}

/// Line-by-line code equality ignoring trailing whitespace — the tolerance
/// a no-op suggestion hides behind.
fn code_lines_equal(a: &str, b: &str) -> bool {
    fn normalize(s: &str) -> Vec<&str> {
        s.lines().map(str::trim_end).collect()
    }
    normalize(a) == normalize(b)
}

/// Map the AI's `CATEGORY:` value onto the canonical category name, tolerating
/// case/spacing drift. [`category_emoji`] derives the emoji shown for it. An
/// unrecognized non-empty value passes through as-is (better an odd label than
/// a lost finding); empty falls back to the most generic category.
fn normalize_review_category(raw: &str) -> String {
    let key: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    match key.as_str() {
        "codesmell" | "cleancode" | "smell" => "Code Smell".to_string(),
        "security" => "Security".to_string(),
        "performance" => "Performance".to_string(),
        "testquality" | "test" | "tests" | "testing" => "Test".to_string(),
        "convention" | "conventions" => "Convention".to_string(),
        _ if raw.trim().is_empty() => "Code Smell".to_string(),
        _ => raw.trim().to_string(),
    }
}

/// Emoji shown for a review category, keyed on the canonical name from
/// [`normalize_review_category`]. The summary's "Type" column shows the emoji
/// alone; the inline comment badge pairs it with the name.
fn category_emoji(category: &str) -> &'static str {
    match category {
        "Security" => "🛡️",
        "Performance" => "🚀",
        "Test" => "🧪",
        "Convention" => "🤝",
        "Code Smell" => "🧹",
        _ => "🏷️",
    }
}

/// Build the review-summary markdown from the findings that were actually
/// posted — a fixed template over structured data, zero AI involvement.
/// Findings that share an issue title collapse into one row so the table stays
/// scannable; the "Where?" cell then lists each location the issue occurs at.
pub fn build_review_summary(posted: &[ReviewFinding]) -> String {
    let groups = group_findings_by_issue(posted);
    let noun = if groups.len() == 1 { "issue" } else { "issues" };
    let mut body = format!(
        "## Review Summary\n\nI reviewed this PR and found {} {noun} that should be \
         addressed before merge.\n\n### Requested Improvements\n\n\
         | Type | Issue | Level | Where? |\n\
         | --- | --- | --- | --- |\n",
        groups.len()
    );
    for group in &groups {
        // Each location is one code span. Kept whole (not hard-broken): GitHub
        // lets a long code span set the column width and keeps the narrow
        // columns at their natural size, so their short labels don't wrap.
        let where_cell = group
            .locations
            .iter()
            .map(|loc| format!("`{}`", md_table_cell(loc)))
            .collect::<Vec<_>>()
            .join("<br>");
        body.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            category_emoji(&group.category),
            md_table_cell(&group.title),
            md_table_cell(group.severity.label()),
            where_cell,
        ));
    }
    body
}

/// One summary-table row: an issue (by title) with every location it was
/// posted at. Distinct findings that share a title merge into a single group.
struct IssueGroup {
    category: String,
    title: String,
    severity: ReviewSeverity,
    locations: Vec<String>,
}

/// Collapse posted findings into one [`IssueGroup`] per distinct title (case-
/// and whitespace-insensitive), preserving first-appearance order. Each group
/// keeps the highest severity seen and accumulates its distinct locations.
fn group_findings_by_issue(posted: &[ReviewFinding]) -> Vec<IssueGroup> {
    let mut groups: Vec<IssueGroup> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for finding in posted {
        let key = finding
            .title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let descriptor = finding.descriptor();
        match index.get(&key) {
            Some(&i) => {
                let group = &mut groups[i];
                if !group.locations.contains(&descriptor) {
                    group.locations.push(descriptor);
                }
                if finding.severity.rank() < group.severity.rank() {
                    group.severity = finding.severity;
                }
            }
            None => {
                index.insert(key, groups.len());
                groups.push(IssueGroup {
                    category: finding.category.clone(),
                    title: finding.title.trim().to_string(),
                    severity: finding.severity,
                    locations: vec![descriptor],
                });
            }
        }
    }
    groups
}

/// Flatten a value into a single GitHub-table cell: collapse every run of
/// whitespace (newlines included) to one space and escape pipes so they stay
/// inside the column instead of splitting it.
fn md_table_cell(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// Build the commit subject + body for one applied review fix, in the format
/// the resolution flow uses: a `fix (review):` subject and a body citing the
/// PR + comment and the reviewer's feedback.
fn format_commit_message(
    number: u64,
    comment_index: usize,
    brief: &str,
    plan: &FixPlan,
) -> (String, String) {
    let summary = {
        let s = plan.summary.trim();
        if s.is_empty() {
            "address review comment".to_string()
        } else {
            s.to_string()
        }
    };
    let subject = format!("fix (review): {summary}");
    let explanation = plan
        .explanation
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| summary.clone());
    let body = format!("PR #{number}, comment #{comment_index} — \"{brief}\"\n{explanation}");
    (subject, body)
}

/// The reply posted when the apply step produced no change because the code
/// already satisfies the comment (the planner over-judged it as actionable).
const ALREADY_RESOLVED_REPLY: &str = "The current code already addresses this — \
    on a closer look against the comment, no change was needed here. Thanks for the feedback!";

/// Prefix of the reply [`format_reply`] posts after committing a fix. Shared
/// with [`is_resolution_reply`] so the thread filter recognises our own
/// resolution reply when deciding whether a thread is still pending.
const ADDRESSED_REPLY_PREFIX: &str = "Addressed in ";

/// Build the reply posted to the reviewer after a fix is committed.
fn format_reply(commit_url: &str, plan: &FixPlan) -> String {
    let summary = {
        let s = plan.summary.trim();
        if s.is_empty() {
            "Applied the suggested change".to_string()
        } else {
            s.to_string()
        }
    };
    format!("{ADDRESSED_REPLY_PREFIX}{commit_url} — {summary}. Thanks for the feedback!")
}

/// True when `body` is one of the resolution replies the Fix loop posts to mark
/// a thread handled: the post-commit "Addressed in <url> …" reply or the
/// "already addresses this — no change needed" reply. The thread filter skips a
/// thread only when such a reply is its most recent comment; if the reviewer
/// responded afterwards, the thread is pending again and gets re-analysed.
fn is_resolution_reply(body: &str) -> bool {
    let body = body.trim();
    body.starts_with(ADDRESSED_REPLY_PREFIX) || body == ALREADY_RESOLVED_REPLY
}

/// Reply to a reviewer: an inline thread reply when the group has an anchor
/// comment, else a general PR comment. Shared by the actionable (commit) and
/// non-actionable (reply-only) paths.
async fn post_reply_internal(
    gh_binary: &Path,
    cwd: &Path,
    owner: &str,
    repo: &str,
    number: u64,
    group: &CommentGroup,
    text: &str,
) -> std::result::Result<String, String> {
    match group.reply_comment_id {
        Some(id) => {
            let endpoint = format!("repos/{owner}/{repo}/pulls/{number}/comments/{id}/replies");
            let body_arg = format!("body={text}");
            time::timeout(
                FIX_REPLY_TIMEOUT,
                run_command(gh_binary, &["api", &endpoint, "-f", &body_arg], Some(cwd)),
            )
            .await
            .map_err(|_| "gh reply timed out".to_string())?
        }
        None => {
            let number_arg = number.to_string();
            time::timeout(
                FIX_REPLY_TIMEOUT,
                run_command(
                    gh_binary,
                    &["pr", "comment", &number_arg, "--body", text],
                    Some(cwd),
                ),
            )
            .await
            .map_err(|_| "gh pr comment timed out".to_string())?
        }
    }
}

/// First 8 chars of a commit hash, for terse error messages.
fn short_hash(hash: &str) -> String {
    hash.trim().chars().take(8).collect()
}

/// Clip `s` to at most `max` chars, appending an ellipsis when truncated.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Truncate `text` to at most `max_bytes` on a UTF-8 char boundary,
/// appending a marker so the reader knows the content was clipped.
fn truncate_for_prompt(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[... diff truncated at ~{} KB — describe the overall change; \
         the full diff exceeds the prompt budget ...]",
        &text[..end],
        max_bytes / 1000
    )
}

/// Parse a drafted `pull_request.md`: the first non-empty line is the PR
/// title (any leading `# ` is stripped), everything after it (leading blank
/// lines trimmed) is the body, and any `<!-- wisetree-labels: ... -->` comment
/// is extracted as the label list (and stripped from the body).
/// Returns `None` when there is no title.
pub fn parse_pull_request_md(content: &str) -> Option<(String, String, Vec<String>)> {
    let mut lines = content.lines();
    let title_line = lines.by_ref().find(|line| !line.trim().is_empty())?;
    let title = title_line.trim().trim_start_matches('#').trim().to_string();
    if title.is_empty() {
        return None;
    }
    let raw_body = lines.collect::<Vec<_>>().join("\n");
    let raw_body = raw_body.trim_start().to_string();
    let (body, labels) = extract_wisetree_labels(&raw_body);
    Some((title, body, labels))
}

/// Extract `<!-- wisetree-labels: label1, label2 -->` from a PR body,
/// returning the cleaned body (comment stripped) and the parsed label list.
fn extract_wisetree_labels(body: &str) -> (String, Vec<String>) {
    static LABELS_COMMENT: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?m)^[ \t]*<!--\s*wisetree-labels:\s*([^-]*?)-->\s*\n?").unwrap()
    });
    match LABELS_COMMENT.captures(body) {
        Some(cap) => {
            let labels_str = cap[1].trim();
            let labels = if labels_str.is_empty() {
                vec![]
            } else {
                labels_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            };
            let clean = LABELS_COMMENT.replace(body, "").trim_start().to_string();
            (clean, labels)
        }
        None => (body.to_string(), vec![]),
    }
}

/// Pull every media reference out of a PR body in first-seen order:
/// markdown images, `<img>`/`<video>` tags, and bare GitHub asset URLs
/// (`.../assets/...`, `user-images.githubusercontent.com/...`). Used to
/// carry screenshots/videos across a description rewrite.
fn extract_media(body: &str) -> Vec<String> {
    static MEDIA: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?is)!\[[^\]]*\]\([^)]*\)|<img[^>]*>|<video[^>]*>.*?</video>|<video[^>]*/?>|https?://github\.com/[^\s)]+/assets/[^\s)]+|https?://user-images\.githubusercontent\.com/[^\s)]+",
        )
        .unwrap()
    });
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in MEDIA.find_iter(body) {
        let snippet = m.as_str().trim().to_string();
        if !snippet.is_empty() && seen.insert(snippet.clone()) {
            out.push(snippet);
        }
    }
    out
}

/// Re-insert media from `old_body` into `new_body` immediately under the
/// `# Overview` heading, skipping anything already present. When the new
/// body has no Overview section the media is appended under a fresh one.
fn preserve_media(old_body: &str, new_body: &str) -> String {
    let fresh: Vec<String> = extract_media(old_body)
        .into_iter()
        .filter(|m| !new_body.contains(m.as_str()))
        .collect();
    if fresh.is_empty() {
        return new_body.to_string();
    }
    let block = fresh.join("\n\n");
    let lines: Vec<&str> = new_body.lines().collect();
    let overview = lines.iter().position(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with('#') && trimmed.to_lowercase().contains("overview")
    });
    match overview {
        Some(idx) => {
            let insert_at = idx + 1;
            let mut result = lines[..insert_at].join("\n");
            result.push_str("\n\n");
            result.push_str(&block);
            if insert_at < lines.len() {
                result.push('\n');
                result.push('\n');
                result.push_str(&lines[insert_at..].join("\n"));
            } else {
                result.push('\n');
            }
            result
        }
        None => format!("{}\n\n# Overview 🔍\n\n{block}\n", new_body.trim_end()),
    }
}

/// First http(s) URL on the last non-empty line of `gh pr create` output —
/// gh prints the new PR's URL as the final line on success.
fn pr_url_from_output(output: &str) -> String {
    output
        .lines()
        .rev()
        .find_map(|line| {
            line.split_whitespace()
                .find(|token| token.starts_with("http"))
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// Parse the numeric PR id from a `.../pull/<n>` GitHub URL.
fn pr_number_from_url(url: &str) -> Option<u64> {
    let tail = url.rsplit("/pull/").next()?;
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().ok()
}

pub fn default_dashboard_warning(config: &DashboardConfig, gh_available: bool) -> Option<String> {
    if config.wise_merge && !config.show_pull_requests {
        Some("Wise Merge requires showPullRequests=true; automatic merge paused.".to_string())
    } else if config.show_pull_requests && !gh_available {
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
    use crate::services::develop::{render_plan_md, PlanSection};

    #[test]
    fn login_shell_check_uses_login_mode_for_supported_shell() {
        let (shell, args) = login_shell_check_command_for_shell("cargo test", Some("/bin/zsh"));

        assert_eq!(shell, PathBuf::from("/bin/zsh"));
        assert_eq!(args, ["-l", "-c", "cargo test"]);
    }

    #[test]
    fn login_shell_check_omits_login_mode_for_unsupported_shell() {
        let (shell, args) = login_shell_check_command_for_shell("cargo test", Some("/usr/bin/nu"));

        assert_eq!(shell, PathBuf::from("/usr/bin/nu"));
        assert_eq!(args, ["-c", "cargo test"]);
    }

    #[test]
    fn login_shell_check_falls_back_for_empty_shell() {
        let (shell, args) = login_shell_check_command_for_shell("cargo test", Some(""));

        assert_eq!(shell, PathBuf::from("/bin/sh"));
        assert_eq!(args, ["-c", "cargo test"]);
    }

    #[test]
    fn login_shell_check_falls_back_for_missing_shell() {
        let (shell, args) = login_shell_check_command_for_shell("cargo test", None);

        assert_eq!(shell, PathBuf::from("/bin/sh"));
        assert_eq!(args, ["-c", "cargo test"]);
    }

    #[test]
    fn develop_plan_prompt_renders_first_run_with_unresolved_base() {
        let prompt = build_develop_plan_prompt("Add dashboard filtering", None, None, None);

        assert!(prompt.contains("Add dashboard filtering"));
        assert!(prompt.contains("(none resolved)"));
        assert!(!prompt.contains("TASK_DESCRIPTION"));
        assert!(!prompt.contains("BASE_REF"));
        assert!(!prompt.contains("PREVIOUS_PLAN"));
        assert!(!prompt.contains("USER_FEEDBACK"));
    }

    #[test]
    fn develop_plan_prompt_renders_revision_context() {
        let prompt = build_develop_plan_prompt(
            "Add dashboard filtering",
            Some("origin/main"),
            Some("## Section 1\nImplement the filter"),
            Some("Keep the existing keyboard shortcut"),
        );

        assert!(prompt.contains("Add dashboard filtering"));
        assert!(prompt.contains("origin/main"));
        assert!(prompt.contains("## Section 1\nImplement the filter"));
        assert!(prompt.contains("Keep the existing keyboard shortcut"));
        assert!(!prompt.contains("TASK_DESCRIPTION"));
        assert!(!prompt.contains("BASE_REF"));
        assert!(!prompt.contains("PREVIOUS_PLAN"));
        assert!(!prompt.contains("USER_FEEDBACK"));
    }

    #[test]
    fn develop_plan_prompt_caps_multibyte_inputs() {
        let oversized_task = "🦀".repeat(DEVELOP_TASK_MAX_BYTES);
        let oversized_plan = "🦀".repeat(DEVELOP_PLAN_MAX_BYTES);
        let oversized_feedback = "🦀".repeat(DEVELOP_FEEDBACK_MAX_BYTES);

        let prompt = build_develop_plan_prompt(
            &oversized_task,
            Some("main"),
            Some(&oversized_plan),
            Some(&oversized_feedback),
        );

        let expected_task = truncate_bugkill_field(&oversized_task, DEVELOP_TASK_MAX_BYTES);
        let expected_plan = truncate_bugkill_field(&oversized_plan, DEVELOP_PLAN_MAX_BYTES);
        let expected_feedback =
            truncate_bugkill_field(&oversized_feedback, DEVELOP_FEEDBACK_MAX_BYTES);

        assert!(prompt.contains(&expected_task));
        assert!(prompt.contains(&expected_plan));
        assert!(prompt.contains(&expected_feedback));
        assert!(!prompt.contains(&oversized_task));
        assert!(!prompt.contains(&oversized_plan));
        assert!(!prompt.contains(&oversized_feedback));
        assert!(expected_task.is_char_boundary(expected_task.len()));
        assert!(expected_plan.is_char_boundary(expected_plan.len()));
        assert!(expected_feedback.is_char_boundary(expected_feedback.len()));
        assert!(expected_task.len() <= DEVELOP_TASK_MAX_BYTES);
        assert!(expected_plan.len() <= DEVELOP_PLAN_MAX_BYTES);
        assert!(expected_feedback.len() <= DEVELOP_FEEDBACK_MAX_BYTES);
    }

    // ── Review pipeline: diff split + findings parsing ─────────────────

    const SAMPLE_DIFF: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,4 +10,5 @@ fn setup() {
 let a = 1;
-let b = 2;
+let b = compute();
+let c = 3;
 finish(a);
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-old
-lines
diff --git a/logo.png b/logo.png
index 333..444 100644
Binary files a/logo.png and b/logo.png differ
diff --git a/new.rs b/new.rs
new file mode 100644
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,2 @@
+fn fresh() {}
+fn more() {}
";

    #[test]
    fn parse_review_diff_splits_files_and_numbers_new_side_lines() {
        let files = parse_review_diff(SAMPLE_DIFF);
        // Deleted + binary files are skipped: only the modified and the new
        // file survive.
        assert_eq!(files.len(), 2, "{files:?}");

        let lib = &files[0];
        assert_eq!(lib.path, "src/lib.rs");
        // Context + added lines are commentable; the removed line is not.
        assert_eq!(
            lib.commentable_lines,
            BTreeSet::from([10, 11, 12, 13]),
            "{lib:?}"
        );
        // The annotated diff numbers the new side and leaves removals bare.
        assert!(lib.annotated_diff.contains("    10  let a = 1;"), "{lib:?}");
        assert!(lib.annotated_diff.contains("    11 +let b = compute();"));
        assert!(lib.annotated_diff.contains("    12 +let c = 3;"));
        assert!(lib.annotated_diff.contains("       -let b = 2;"));

        let new = &files[1];
        assert_eq!(new.path, "new.rs");
        assert_eq!(new.commentable_lines, BTreeSet::from([1, 2]));
    }

    #[test]
    fn parse_review_diff_returns_empty_for_no_text_changes() {
        assert!(parse_review_diff("").is_empty());
        let binary_only = "diff --git a/x.png b/x.png\nBinary files a/x.png and b/x.png differ\n";
        assert!(parse_review_diff(binary_only).is_empty());
    }

    #[test]
    fn parse_review_findings_reads_multiple_findings() {
        let out = "chatter\n===WISETREE-REVIEW-BEGIN===\n\
            ---FINDING---\n\
            CATEGORY: Security\n\
            SEVERITY: Critical\n\
            LINE: 11\n\
            START_LINE:\n\
            TITLE: Unvalidated compute() input\n\
            ---EXPLANATION---\n\
            compute() feeds user input straight into the query.\n\
            ---SUGGESTION---\n\
            let b = compute_checked()?;\n\
            ---END-FINDING---\n\
            ---FINDING---\n\
            CATEGORY: code smell\n\
            SEVERITY: low\n\
            LINE: 12\n\
            TITLE: Magic number\n\
            ---EXPLANATION---\n\
            3 carries no meaning.\n\
            ---END-FINDING---\n\
            ===WISETREE-REVIEW-END===\ntrailing";
        let commentable = BTreeSet::from([10, 11, 12]);
        let findings = parse_review_findings(out, "src/lib.rs", &commentable, "").unwrap();
        assert_eq!(findings.len(), 2, "{findings:?}");

        let first = &findings[0];
        assert_eq!(first.category, "Security");
        assert_eq!(first.severity, ReviewSeverity::Critical);
        assert_eq!(first.file, "src/lib.rs");
        assert_eq!(first.line, Some(11));
        assert_eq!(first.start_line, None);
        assert_eq!(first.title, "Unvalidated compute() input");
        assert_eq!(
            first.suggestion.as_deref(),
            Some("let b = compute_checked()?;")
        );

        // Case-insensitive category + severity normalization.
        let second = &findings[1];
        assert_eq!(second.category, "Code Smell");
        assert_eq!(second.severity, ReviewSeverity::Low);
        assert!(second.suggestion.is_none());
    }

    #[test]
    fn parse_review_findings_accepts_a_clean_scan() {
        let out = "===WISETREE-REVIEW-BEGIN===\nNO-FINDINGS\n===WISETREE-REVIEW-END===";
        let findings = parse_review_findings(out, "a.rs", &BTreeSet::new(), "").unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn parse_review_findings_rejects_output_without_markers() {
        assert!(parse_review_findings("no block at all", "a.rs", &BTreeSet::new(), "").is_none());
    }

    #[test]
    fn parse_review_findings_downgrades_invalid_line_anchors() {
        // Line 99 is not in the diff → the finding must become file-level
        // instead of letting GitHub reject the comment with a 422.
        let out = "===WISETREE-REVIEW-BEGIN===\n\
            ---FINDING---\n\
            CATEGORY: Performance\n\
            SEVERITY: High\n\
            LINE: 99\n\
            TITLE: N+1 query\n\
            ---EXPLANATION---\n\
            Loads one row per iteration.\n\
            ---END-FINDING---\n\
            ===WISETREE-REVIEW-END===";
        let commentable = BTreeSet::from([1, 2, 3]);
        let findings = parse_review_findings(out, "a.rs", &commentable, "").unwrap();
        assert_eq!(findings[0].line, None);
        assert_eq!(findings[0].start_line, None);
    }

    #[test]
    fn parse_review_findings_validates_start_line_ranges() {
        // A valid range keeps both anchors; an inverted or unknown start
        // drops only the start (single-line comment survives).
        let block = |start: &str| {
            format!(
                "===WISETREE-REVIEW-BEGIN===\n---FINDING---\nCATEGORY: Security\n\
                 SEVERITY: High\nLINE: 5\nSTART_LINE: {start}\nTITLE: t\n\
                 ---EXPLANATION---\ne\n---END-FINDING---\n===WISETREE-REVIEW-END==="
            )
        };
        let commentable = BTreeSet::from([3, 4, 5]);
        let ok = parse_review_findings(&block("3"), "a.rs", &commentable, "").unwrap();
        assert_eq!((ok[0].start_line, ok[0].line), (Some(3), Some(5)));
        let inverted = parse_review_findings(&block("9"), "a.rs", &commentable, "").unwrap();
        assert_eq!((inverted[0].start_line, inverted[0].line), (None, Some(5)));
    }

    #[test]
    fn review_finding_comment_body_renders_inline_suggestion() {
        let finding = ReviewFinding {
            category: "Security".to_string(),
            severity: ReviewSeverity::Critical,
            file: "src/auth.rs".to_string(),
            start_line: None,
            line: Some(42),
            title: "Hardcoded API key".to_string(),
            explanation: "Secrets in source leak through history.".to_string(),
            suggestion: Some("let key = env::var(\"API_KEY\")?;".to_string()),
        };
        let body = finding.comment_body();
        // Title leads as a heading; category/severity moved to a centered footer.
        assert!(body.starts_with("### Hardcoded API key"));
        assert!(body.contains("Secrets in source leak through history."));
        assert!(body.contains("```suggestion\nlet key = env::var(\"API_KEY\")?;\n```"));
        assert!(body.ends_with("<p align=\"center\">\n🔴 [🛡️ Security] [Critical]\n</p>"));
        // Inline comments don't repeat the path — GitHub anchors them.
        assert!(!body.contains("📄"));
    }

    #[test]
    fn review_finding_comment_body_downgrades_file_level_suggestions() {
        let finding = ReviewFinding {
            category: "Test".to_string(),
            severity: ReviewSeverity::High,
            file: "src/auth.rs".to_string(),
            start_line: None,
            line: None,
            title: "Missing failure-path test".to_string(),
            explanation: "The invalid-token flow is untested.".to_string(),
            suggestion: Some("assert!(auth(bad).is_err());".to_string()),
        };
        let body = finding.comment_body();
        assert!(body.starts_with("### Missing failure-path test"));
        // File-level comments name the file and must NOT use a ```suggestion
        // block (those only work anchored to diff lines).
        assert!(body.contains("📄 `src/auth.rs`"));
        assert!(!body.contains("```suggestion"));
        assert!(body.contains("**Proposed code:**\n```\nassert!(auth(bad).is_err());\n```"));
        assert!(body.ends_with("<p align=\"center\">\n🟠 [🧪 Test] [High]\n</p>"));
    }

    #[test]
    fn build_review_summary_lists_the_issue_title_only() {
        let posted = vec![ReviewFinding {
            category: "Performance".to_string(),
            severity: ReviewSeverity::High,
            file: "src/db.rs".to_string(),
            start_line: None,
            line: Some(23),
            title: "N+1 query in loop".to_string(),
            explanation: "each iteration hits the DB again".to_string(),
            suggestion: None,
        }];
        let body = build_review_summary(&posted);
        assert!(body.contains("## Review Summary"));
        // Singular noun, no robotic "(s)".
        assert!(body.contains("found 1 issue that should be addressed"));
        assert!(!body.contains("issue(s)"));
        // "Issue" column, title only — the explanation stays out of the table.
        assert!(body.contains("| Type | Issue | Level | Where? |"));
        // The Type column shows the category emoji alone.
        assert!(body.contains("| 🚀 | N+1 query in loop | High | `src/db.rs:23` |"));
        assert!(!body.contains("each iteration hits the DB again"));
        // The redundant Notes section is gone.
        assert!(!body.contains("### Notes"));
    }

    #[test]
    fn build_review_summary_groups_shared_issues_and_lists_each_location() {
        let make = |file: &str, line: u64, severity: ReviewSeverity| ReviewFinding {
            category: "Test".to_string(),
            severity,
            file: file.to_string(),
            start_line: None,
            line: Some(line),
            title: "Missing coverage".to_string(),
            explanation: String::new(),
            suggestion: None,
        };
        let posted = vec![
            make("src/a.rs", 12, ReviewSeverity::Low),
            make("src/b.rs", 40, ReviewSeverity::High),
        ];
        let body = build_review_summary(&posted);
        // Two findings, one shared issue → one row, counted as a single issue.
        assert!(body.contains("found 1 issue that should be addressed"));
        // Both locations in one cell, each on its own line; highest severity wins.
        assert!(body.contains("| 🧪 | Missing coverage | High | `src/a.rs:12`<br>`src/b.rs:40` |"));
    }

    #[test]
    fn build_review_summary_pluralizes_and_escapes_table_cells() {
        let posted = vec![
            ReviewFinding {
                category: "Security".to_string(),
                severity: ReviewSeverity::Critical,
                file: "src/a.rs".to_string(),
                start_line: None,
                line: Some(1),
                // A pipe in the title must not break the table layout.
                title: "Unsanitized `a | b` input".to_string(),
                explanation: String::new(),
                suggestion: None,
            },
            ReviewFinding {
                category: "Code Smell".to_string(),
                severity: ReviewSeverity::Low,
                file: "src/b.rs".to_string(),
                start_line: Some(4),
                line: Some(6),
                title: "Dead branch".to_string(),
                explanation: String::new(),
                suggestion: None,
            },
        ];
        let body = build_review_summary(&posted);
        // Two distinct issues → plural noun.
        assert!(body.contains("found 2 issues that should be addressed"));
        // Pipe in the title escaped.
        assert!(body.contains("Unsanitized `a \\| b` input"));
        // A line range renders in the Where? cell; the Type shows the emoji alone.
        assert!(body.contains("| 🧹 | Dead branch | Low | `src/b.rs:4-6` |"));
    }

    #[test]
    fn build_review_summary_keeps_a_long_path_as_one_code_span() {
        let posted = vec![ReviewFinding {
            category: "Test".to_string(),
            severity: ReviewSeverity::Low,
            file: "lib/components/attachment/attachment_viewer_content_desktop.dart".to_string(),
            start_line: None,
            line: Some(155),
            title: "Hidden mode untested".to_string(),
            explanation: String::new(),
            suggestion: None,
        }];
        let body = build_review_summary(&posted);
        // The full path stays in a single code span (no hard breaks inside it),
        // which renders cleanly without wrapping the narrow columns' labels.
        assert!(body.contains(
            "| `lib/components/attachment/attachment_viewer_content_desktop.dart:155` |"
        ));
    }

    #[test]
    fn normalize_review_category_returns_canonical_names() {
        // The various aliases the AI may emit collapse onto one canonical name.
        assert_eq!(normalize_review_category("Test Quality"), "Test");
        assert_eq!(normalize_review_category("testing"), "Test");
        assert_eq!(normalize_review_category("code smell"), "Code Smell");
        assert_eq!(normalize_review_category("security"), "Security");
        assert_eq!(normalize_review_category("Performance"), "Performance");
        assert_eq!(normalize_review_category("convention"), "Convention");
    }

    #[test]
    fn category_emoji_maps_each_canonical_name() {
        // The summary "Type" column shows this emoji alone; the comment badge
        // pairs it with the name.
        assert_eq!(category_emoji("Test"), "🧪");
        assert_eq!(category_emoji("Code Smell"), "🧹");
        assert_eq!(category_emoji("Performance"), "🚀");
        assert_eq!(category_emoji("Convention"), "🤝");
        assert_eq!(category_emoji("Security"), "🛡️");
    }

    #[test]
    fn parse_existing_review_comments_groups_by_path() {
        let json = r#"[
            {"path": "a.rs", "line": 7, "body": "prefer a constant", "user": {"login": "alice"}},
            {"path": "a.rs", "original_line": 9, "body": "typo", "user": {"login": "bob"}},
            {"path": "b.rs", "body": "structure question", "user": {"login": "alice"}},
            {"path": "c.rs", "body": "   ", "user": {"login": "alice"}}
        ]"#;
        let by_path = parse_existing_review_comments(json);
        let a = by_path.get("a.rs").unwrap();
        assert!(a.rendered.contains("@alice (line 7): prefer a constant"));
        assert!(a.rendered.contains("@bob (line 9): typo"));
        assert!(by_path
            .get("b.rs")
            .unwrap()
            .rendered
            .contains("@alice: structure"));
        // Blank bodies contribute nothing.
        assert!(!by_path.contains_key("c.rs"));
        // Garbage input degrades to "no context", never an error.
        assert!(parse_existing_review_comments("not json").is_empty());
        // Human comments never produce dedup keys.
        assert!(a.keys.is_empty());
    }

    #[test]
    fn wisetree_comments_produce_dedup_keys_humans_do_not() {
        // Round-trip the *actual* posted comment body through the parser so the
        // dedup key can never silently drift from the format the command emits.
        let finding = ReviewFinding {
            category: "Security".to_string(),
            severity: ReviewSeverity::High,
            file: "a.rs".to_string(),
            start_line: None,
            line: Some(7),
            title: "Hardcoded API key".to_string(),
            explanation: "Secrets leak.".to_string(),
            suggestion: None,
        };
        let json = serde_json::json!([
            {"path": "a.rs", "line": 7, "body": finding.comment_body(),
             "user": {"login": "wisetree"}},
            {"path": "a.rs", "line": 9, "body": "please rename this", "user": {"login": "bob"}}
        ])
        .to_string();
        let by_path = parse_existing_review_comments(&json);
        assert_eq!(
            by_path.get("a.rs").unwrap().keys,
            vec![ExistingFindingKey {
                line: Some(7),
                title: "hardcoded api key".to_string(),
            }]
        );
    }

    #[test]
    fn wisetree_finding_title_still_reads_legacy_header() {
        // Comments posted before the format change must keep producing keys.
        assert_eq!(
            wisetree_finding_title("**[Security] [High]**: Hardcoded API key\n\nSecrets leak."),
            Some("hardcoded api key".to_string())
        );
        // A plain human heading with no badge is not one of ours.
        assert_eq!(
            wisetree_finding_title("### Just my two cents\n\nlooks fine"),
            None
        );
    }

    #[test]
    fn split_duplicate_findings_drops_only_matching_line_and_title() {
        let finding = |line: Option<u64>, title: &str| ReviewFinding {
            category: "Security".to_string(),
            severity: ReviewSeverity::High,
            file: "a.rs".to_string(),
            start_line: None,
            line,
            title: title.to_string(),
            explanation: "why".to_string(),
            suggestion: None,
        };
        let existing = vec![ExistingFindingKey {
            line: Some(7),
            title: "hardcoded api key".to_string(),
        }];
        let (fresh, duplicates) = split_duplicate_findings(
            vec![
                finding(Some(7), "Hardcoded API key"), // same line + title → dup
                finding(Some(9), "Hardcoded API key"), // other line → fresh
                finding(Some(7), "Magic number"),      // other title → fresh
            ],
            &existing,
        );
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].line, Some(7));
        assert_eq!(fresh.len(), 2);
        // No keys → everything is fresh.
        let (all, none) = split_duplicate_findings(vec![finding(Some(7), "x")], &[]);
        assert_eq!((all.len(), none.len()), (1, 0));
    }

    fn run_finding(
        file: &str,
        line: Option<u64>,
        title: &str,
        suggestion: Option<&str>,
    ) -> ReviewFinding {
        ReviewFinding {
            category: "Security".to_string(),
            severity: ReviewSeverity::High,
            file: file.to_string(),
            start_line: None,
            line,
            title: title.to_string(),
            explanation: "why".to_string(),
            suggestion: suggestion.map(str::to_string),
        }
    }

    #[test]
    fn run_dedup_collapses_same_fix_even_on_different_lines() {
        // Same proposed fix in the same file, worded differently and anchored
        // to different lines: the second is a duplicate.
        let (kept, dups) = split_run_duplicate_findings(vec![
            run_finding("a.rs", Some(4), "Use env var", Some("let k = env(\"K\");")),
            run_finding(
                "a.rs",
                Some(9),
                "Read the key from the environment",
                Some("let k = env(\"K\");"),
            ),
        ]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].line, Some(4)); // the first (highest-priority) one
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].line, Some(9));
    }

    #[test]
    fn run_dedup_ignores_whitespace_only_differences_in_the_fix() {
        let (kept, dups) = split_run_duplicate_findings(vec![
            run_finding("a.rs", Some(4), "A", Some("let k = env(\"K\");")),
            run_finding("a.rs", Some(9), "B", Some("  let   k = env(\"K\");\n")),
        ]);
        assert_eq!(kept.len(), 1);
        assert_eq!(dups.len(), 1);
    }

    #[test]
    fn run_dedup_collapses_same_line_regardless_of_fix() {
        // Two findings on the same anchor collapse even without a shared fix.
        let (kept, dups) = split_run_duplicate_findings(vec![
            run_finding("a.rs", Some(7), "Naming", None),
            run_finding("a.rs", Some(7), "Magic number", Some("const N = 3;")),
        ]);
        assert_eq!(kept.len(), 1);
        assert_eq!(dups.len(), 1);
    }

    #[test]
    fn run_dedup_keeps_distinct_findings() {
        // Different files with the same fix, and distinct fixes on distinct
        // lines — none are duplicates.
        let (kept, dups) = split_run_duplicate_findings(vec![
            run_finding("a.rs", Some(4), "A", Some("same();")),
            run_finding("b.rs", Some(4), "B", Some("same();")), // other file
            run_finding("a.rs", Some(9), "C", Some("different();")),
            run_finding("a.rs", None, "D", None), // file-level, no fix key
        ]);
        assert_eq!(kept.len(), 4);
        assert!(dups.is_empty());
    }

    #[test]
    fn noop_suggestions_are_stripped_but_real_ones_survive() {
        let diff = "     6 +let a = 1;\n     7 +let key = \"abc\";\n     8  }";
        let commentable = BTreeSet::from([6, 7, 8]);
        let block = |suggestion: &str| {
            format!(
                "===WISETREE-REVIEW-BEGIN===\n---FINDING---\nCATEGORY: Security\nSEVERITY: High\n\
                 LINE: 7\nSTART_LINE:\nTITLE: Hardcoded key\n---EXPLANATION---\nwhy\n\
                 ---SUGGESTION---\n{suggestion}\n---END-FINDING---\n===WISETREE-REVIEW-END==="
            )
        };
        // Reproduces line 7 verbatim (modulo trailing spaces) → no-op, stripped.
        let noop =
            parse_review_findings(&block("let key = \"abc\";  "), "a.rs", &commentable, diff)
                .unwrap();
        assert_eq!(noop[0].suggestion, None);
        assert_eq!(noop[0].title, "Hardcoded key"); // the finding itself survives
                                                    // An actual change is kept.
        let real = parse_review_findings(
            &block("let key = env(\"KEY\");"),
            "a.rs",
            &commentable,
            diff,
        )
        .unwrap();
        assert_eq!(
            real[0].suggestion.as_deref(),
            Some("let key = env(\"KEY\");")
        );
    }

    #[test]
    fn noop_check_covers_multi_line_ranges() {
        let diff = "     6 +let a = 1;\n     7 +let b = 2;";
        let commentable = BTreeSet::from([6, 7]);
        let out = "===WISETREE-REVIEW-BEGIN===\n---FINDING---\nCATEGORY: Code Smell\n\
                   SEVERITY: Low\nLINE: 7\nSTART_LINE: 6\nTITLE: t\n---EXPLANATION---\ne\n\
                   ---SUGGESTION---\nlet a = 1;\nlet b = 2;\n---END-FINDING---\n\
                   ===WISETREE-REVIEW-END===";
        let findings = parse_review_findings(out, "a.rs", &commentable, diff).unwrap();
        assert_eq!(findings[0].suggestion, None, "{findings:?}");
    }

    #[test]
    fn parse_review_pr_json_reads_slug_head_sha_and_base_ref() {
        let json = r#"{"url": "https://github.com/victorcorcos/wisetree/pull/9", "headRefOid": "abc123", "baseRefName": "main"}"#;
        assert_eq!(
            parse_review_pr_json(json),
            Some((
                "victorcorcos".to_string(),
                "wisetree".to_string(),
                "abc123".to_string(),
                "main".to_string()
            ))
        );
        // A missing head sha is unusable for inline comments.
        assert_eq!(
            parse_review_pr_json(r#"{"url": "https://github.com/o/r/pull/9"}"#),
            None
        );
    }

    #[test]
    fn build_review_scan_prompt_substitutes_every_placeholder() {
        let file = ReviewFile {
            path: "src/lib.rs".to_string(),
            annotated_diff: "@@ -1 +1 @@\n     1 +let x = 1;".to_string(),
            commentable_lines: BTreeSet::from([1]),
            existing_comments: "@alice (line 1): rename this".to_string(),
            existing_keys: Vec::new(),
        };
        let prompt = build_review_scan_prompt(&file, "/tmp/tables.md", None, None);
        assert!(prompt.contains("src/lib.rs"));
        assert!(prompt.contains("     1 +let x = 1;"));
        assert!(prompt.contains("@alice (line 1): rename this"));
        assert!(prompt.contains("/tmp/tables.md"));
        // No unsubstituted tokens survive.
        for token in [
            "FILE_PATH",
            "FILE_DIFF",
            "EXISTING_COMMENTS",
            "TABLES_PATH",
            "USER_FEEDBACK",
            "PREVIOUS_FINDING",
        ] {
            assert!(!prompt.contains(token), "{token} leaked into the prompt");
        }
        // The revision pass threads feedback + the previous finding through.
        let revised = build_review_scan_prompt(&file, "/tmp/t.md", Some("too harsh"), Some("prev"));
        assert!(revised.contains("too harsh"));
        assert!(revised.contains("prev"));
    }

    #[test]
    fn review_skip_reason_filters_unreviewable_files() {
        for (path, reason) in [
            ("Cargo.lock", "lockfile"),
            ("frontend/package-lock.json", "lockfile"),
            ("gradle/deps.lockfile", "lockfile"),
            ("dist/app.min.js", "minified asset"),
            ("dist/app.js.map", "source map"),
            ("src/__snapshots__/app.tsx.snap", "test snapshot"),
            ("components/Button.snap", "test snapshot"),
            ("vendor/lib/util.rb", "vendored code"),
            ("api/types.generated.ts", "generated code"),
            ("proto/events.pb.go", "generated code"),
            ("proto/events_pb2.py", "generated code"),
        ] {
            assert_eq!(review_skip_reason(path), Some(reason), "{path}");
        }
        for path in [
            "src/services/dashboard.rs",
            "Cargo.toml",
            "src/locker.rs",
            "docs/lockfile-strategy.md",
            "src/snapshot.rs",
        ] {
            assert_eq!(review_skip_reason(path), None, "{path}");
        }
    }

    #[test]
    fn partition_reviewable_files_splits_and_keeps_reasons() {
        let file = |path: &str| ReviewFile {
            path: path.to_string(),
            annotated_diff: String::new(),
            commentable_lines: BTreeSet::new(),
            existing_comments: String::new(),
            existing_keys: Vec::new(),
        };
        let (reviewable, skipped) =
            partition_reviewable_files(vec![file("src/lib.rs"), file("Cargo.lock")]);
        assert_eq!(reviewable.len(), 1);
        assert_eq!(reviewable[0].path, "src/lib.rs");
        assert_eq!(
            skipped,
            vec![ReviewSkippedFile {
                path: "Cargo.lock".to_string(),
                reason: "lockfile",
            }]
        );
    }

    #[test]
    fn is_test_file_matches_common_layouts() {
        for path in [
            "tests/tui_update_pr.rs",
            "spec/models/user_spec.rb",
            "src/__tests__/app.tsx",
            "app/services/specs/billing.py",
            "pkg/store_test.go",
            "src/app.test.ts",
            "src/app.spec.js",
            "tests/test_parser.py",
            "conftest.py",
        ] {
            assert!(is_test_file(path), "{path} should be a test file");
        }
        for path in [
            "src/services/dashboard.rs",
            "src/latest.rs",
            "app/contest_controller.rb",
            "docs/testing.md",
            "src/protest/mod.rs",
        ] {
            assert!(!is_test_file(path), "{path} should be a source file");
        }
    }

    #[test]
    fn build_review_scan_prompt_picks_the_profile_by_file_kind() {
        let source = ReviewFile {
            path: "src/lib.rs".to_string(),
            annotated_diff: "     1 +let x = 1;".to_string(),
            commentable_lines: BTreeSet::from([1]),
            existing_comments: String::new(),
            existing_keys: Vec::new(),
        };
        let test = ReviewFile {
            path: "tests/lib_test.rs".to_string(),
            ..source.clone()
        };
        let source_prompt = build_review_scan_prompt(&source, "/tmp/t.md", None, None);
        let test_prompt = build_review_scan_prompt(&test, "/tmp/t.md", None, None);
        assert!(source_prompt.starts_with("You are reviewing the changed lines of ONE file"));
        assert!(test_prompt.starts_with("You are reviewing the changed lines of ONE test file"));
        assert!(test_prompt.contains("test-quality specialist"));
        // Coverage is owned by the whole-diff pass — both per-file profiles
        // must carry the hand-off so parallel scans stop duplicating "add
        // tests" recommendations.
        assert!(source_prompt.contains("Out of scope for this scan: **test coverage**"));
        assert!(test_prompt.contains("whole-diff coverage pass"));
        // Both profiles share the same machine-parsed output contract.
        for prompt in [&source_prompt, &test_prompt] {
            assert!(prompt.contains("===WISETREE-REVIEW-BEGIN==="));
            assert!(prompt.contains("---END-FINDING---"));
        }
    }

    #[test]
    fn build_review_coverage_prompt_sections_every_file() {
        let app = ReviewFile {
            path: "src/lib.rs".to_string(),
            annotated_diff: "     1 +let x = 1;".to_string(),
            commentable_lines: BTreeSet::from([1]),
            existing_comments: "@bob (line 1): please test this".to_string(),
            existing_keys: Vec::new(),
        };
        let test = ReviewFile {
            path: "tests/lib_test.rs".to_string(),
            annotated_diff: "     9 +assert!(x);".to_string(),
            commentable_lines: BTreeSet::from([9]),
            existing_comments: String::new(),
            ..app.clone()
        };
        let prompt = build_review_coverage_prompt(&[app, test]);
        assert!(prompt.starts_with("You are the test-coverage specialist"));
        assert!(prompt.contains("### FILE: src/lib.rs"));
        assert!(prompt.contains("     1 +let x = 1;"));
        assert!(prompt.contains("### FILE: tests/lib_test.rs"));
        assert!(prompt.contains("     9 +assert!(x);"));
        assert!(prompt.contains("@bob (line 1): please test this"));
        assert!(prompt.contains("===WISETREE-REVIEW-BEGIN==="));
    }

    #[test]
    fn parse_coverage_findings_maps_files_and_validates_anchors() {
        let files = vec![
            ReviewFile {
                path: "src/a.rs".to_string(),
                annotated_diff: "     2 +fn run() {}".to_string(),
                commentable_lines: BTreeSet::from([2]),
                existing_comments: String::new(),
                existing_keys: Vec::new(),
            },
            ReviewFile {
                path: "src/b.rs".to_string(),
                annotated_diff: "     5 +fn stop() {}".to_string(),
                commentable_lines: BTreeSet::from([5]),
                existing_comments: String::new(),
                existing_keys: Vec::new(),
            },
        ];
        let output = "\
===WISETREE-REVIEW-BEGIN===
---FINDING---
CATEGORY: Security
SEVERITY: High
FILE: ./src/a.rs
LINE: 2
START_LINE:
TITLE: run() error path untested
---EXPLANATION---
No test fails when run() misbehaves.
---END-FINDING---
---FINDING---
CATEGORY: Test Quality
SEVERITY: Medium
FILE: src/b.rs
LINE: 99
START_LINE:
TITLE: stop() untested
---EXPLANATION---
No test covers stop().
---END-FINDING---
---FINDING---
CATEGORY: Test Quality
SEVERITY: Low
FILE: docs/readme.md
LINE: 1
START_LINE:
TITLE: not a changed file
---EXPLANATION---
Should be dropped.
---END-FINDING---
===WISETREE-REVIEW-END===";
        let findings = parse_coverage_findings(output, &files).unwrap();
        assert_eq!(findings.len(), 2, "the unknown file's finding is dropped");
        // `./` prefix tolerated; the stray CATEGORY is pinned to Test.
        assert_eq!(findings[0].file, "src/a.rs");
        assert_eq!(findings[0].line, Some(2));
        assert_eq!(findings[0].category, "Test");
        // An anchor outside b.rs's commentable lines downgrades to file-level.
        assert_eq!(findings[1].file, "src/b.rs");
        assert_eq!(findings[1].line, None);
    }

    #[test]
    fn parse_coverage_findings_accepts_a_clean_scan() {
        let out = "chatter\n===WISETREE-REVIEW-BEGIN===\nNO-FINDINGS\n===WISETREE-REVIEW-END===";
        assert_eq!(parse_coverage_findings(out, &[]), Some(Vec::new()));
        assert_eq!(parse_coverage_findings("no block at all", &[]), None);
    }

    // ── Fix pipeline: verdict parsing ──────────────────────────────────

    #[test]
    fn parse_fix_verdict_reads_praise() {
        let out =
            "chatter\n===WISETREE-FIX-BEGIN===\nVERDICT: praise\n===WISETREE-FIX-END===\nmore";
        assert_eq!(parse_fix_verdict(out), Some(FixVerdict::Praise));
    }

    #[test]
    fn parse_fix_verdict_reads_reply() {
        let out = "===WISETREE-FIX-BEGIN===\nVERDICT: reply\n---REPLY---\nThe code already guards this case in `init()`.\n===WISETREE-FIX-END===";
        assert_eq!(
            parse_fix_verdict(out),
            Some(FixVerdict::Reply(
                "The code already guards this case in `init()`.".to_string()
            ))
        );
    }

    #[test]
    fn parse_fix_verdict_reads_multiline_fix() {
        let out = "\
===WISETREE-FIX-BEGIN===
VERDICT: fix
---SUMMARY---
extract retry delay into a named constant
---VALIDITY---
Valid: 3000 is a magic number.
---EXPLANATION---
Replace the literal with a documented constant
so the intent reads clearly.
---CHANGE---
- sleep(3000)
+ sleep(RETRY_DELAY_MS)
===WISETREE-FIX-END===";
        let verdict = parse_fix_verdict(out).expect("fix verdict");
        match verdict {
            FixVerdict::Fix(plan) => {
                assert_eq!(plan.summary, "extract retry delay into a named constant");
                assert_eq!(plan.validity, "Valid: 3000 is a magic number.");
                assert!(plan.explanation.contains("documented constant"));
                assert!(plan.explanation.contains("reads clearly"));
                assert!(plan.change.contains("RETRY_DELAY_MS"));
            }
            other => panic!("expected Fix, got {other:?}"),
        }
    }

    #[test]
    fn parse_fix_verdict_rejects_missing_block() {
        assert_eq!(parse_fix_verdict("no markers here at all"), None);
        // Unknown verdict word.
        let out = "===WISETREE-FIX-BEGIN===\nVERDICT: maybe\n===WISETREE-FIX-END===";
        assert_eq!(parse_fix_verdict(out), None);
    }

    // ── Fix pipeline: comment grouping ─────────────────────────────────

    #[test]
    fn group_review_feedback_keeps_inline_and_folds_pr_level() {
        let json = r#"{
          "data": { "repository": { "pullRequest": {
            "reviewThreads": { "nodes": [
              { "isResolved": true, "isOutdated": false, "comments": { "nodes": [
                { "databaseId": 1, "path": "a.rs", "line": 5, "isMinimized": false, "viewerDidAuthor": false, "body": "resolved", "author": { "login": "rev" } }
              ] } },
              { "isResolved": false, "isOutdated": true, "comments": { "nodes": [
                { "databaseId": 2, "path": "a.rs", "line": null, "originalLine": 6, "isMinimized": false, "viewerDidAuthor": false, "body": "outdated but actionable", "author": { "login": "codex" } }
              ] } },
              { "isResolved": false, "isOutdated": false, "comments": { "nodes": [
                { "databaseId": 3, "path": "a.rs", "line": 10, "isMinimized": false, "viewerDidAuthor": false, "body": "rename foo", "author": { "login": "alice" } },
                { "databaseId": 4, "path": "a.rs", "line": 10, "isMinimized": true, "viewerDidAuthor": false, "body": "hidden", "author": { "login": "spam" } }
              ] } },
              { "isResolved": false, "isOutdated": false, "comments": { "nodes": [
                { "databaseId": 5, "path": "a.rs", "line": 10, "isMinimized": false, "viewerDidAuthor": false, "body": "second thread, same line", "author": { "login": "bob" } }
              ] } }
            ] },
            "reviews": { "nodes": [
              { "state": "COMMENTED", "body": "Overall looks solid, one concern below.", "author": { "login": "codex" } },
              { "state": "APPROVED", "body": "", "author": { "login": "ci" } }
            ] }
          } } }
        }"#;
        let groups = parse_and_group_review_feedback(json).expect("parse ok");
        // Resolved dropped; outdated-without-our-reply kept; same-line threads
        // merged; PR-level summary folded into one trailing group (empty review
        // body excluded).
        assert_eq!(groups.len(), 3);

        // Outdated-but-unreplied inline thread is retained, anchored to the
        // inline comment (line falls back to originalLine).
        let outdated = &groups[0];
        assert_eq!(outdated.reply_comment_id, Some(2));
        assert_eq!(outdated.line, Some(6));
        assert_eq!(outdated.comments[0].author, "codex");

        let inline = &groups[1];
        assert_eq!(inline.file.as_deref(), Some("a.rs"));
        assert_eq!(inline.line, Some(10));
        assert_eq!(inline.reply_comment_id, Some(3));
        // Minimized comment dropped; both same-line threads merged.
        assert_eq!(inline.comments.len(), 2);
        assert_eq!(inline.comments[0].author, "alice");
        assert_eq!(inline.comments[1].author, "bob");

        // The PR-level review summary is its own trailing group: not line-
        // anchored and with no inline reply target (reply falls back to a
        // general PR comment).
        let summary = &groups[2];
        assert!(summary.file.is_none());
        assert!(summary.line.is_none());
        assert!(summary.reply_comment_id.is_none());
        assert_eq!(summary.comments.len(), 1);
        assert!(summary.comments[0].body.contains("Overall looks solid"));
    }

    #[test]
    fn group_review_feedback_skips_thread_ending_in_our_resolution_reply() {
        // A thread whose most recent comment is our own resolution reply
        // ("Addressed in …") was handled and nobody objected since, so it is
        // dropped. A second thread with no reply from us is still pending.
        let json = r#"{ "data": { "repository": { "pullRequest": {
            "reviewThreads": { "nodes": [
              { "isResolved": false, "comments": { "nodes": [
                { "databaseId": 1, "path": "a.rs", "line": null, "originalLine": 8, "isMinimized": false, "viewerDidAuthor": false, "body": "extract this", "author": { "login": "alice" } },
                { "databaseId": 2, "path": "a.rs", "line": null, "originalLine": 8, "isMinimized": false, "viewerDidAuthor": true, "body": "Addressed in abc123 — extracted it. Thanks for the feedback!", "author": { "login": "me" } }
              ] } },
              { "isResolved": false, "comments": { "nodes": [
                { "databaseId": 3, "path": "b.rs", "line": null, "originalLine": 3, "isMinimized": false, "viewerDidAuthor": false, "body": "still needs work", "author": { "login": "alice" } }
              ] } }
            ] },
            "reviews": { "nodes": [] }
        } } } }"#;
        let groups = parse_and_group_review_feedback(json).expect("parse ok");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].reply_comment_id, Some(3));
        assert_eq!(groups[0].comments[0].body, "still needs work");
    }

    #[test]
    fn group_review_feedback_reanalyses_thread_with_reviewer_followup() {
        // The reviewer replied *after* our "Addressed in …" resolution reply
        // ("you got it wrong"). The thread is pending again: it is kept and the
        // whole discussion — original comment, our reply, the follow-up — is
        // handed to the planner so it can understand what went wrong.
        let json = r#"{ "data": { "repository": { "pullRequest": {
            "reviewThreads": { "nodes": [
              { "isResolved": false, "comments": { "nodes": [
                { "databaseId": 1, "path": "a.rs", "line": 4, "isMinimized": false, "viewerDidAuthor": false, "body": "This function should take 4 parameters", "author": { "login": "marcos" } },
                { "databaseId": 2, "path": "a.rs", "line": 4, "isMinimized": false, "viewerDidAuthor": true, "body": "Addressed in abc123 — updated the signature. Thanks for the feedback!", "author": { "login": "me" } },
                { "databaseId": 3, "path": "a.rs", "line": 4, "isMinimized": false, "viewerDidAuthor": false, "body": "You changed it to 3 parameters, it should be 4!", "author": { "login": "marcos" } }
              ] } }
            ] },
            "reviews": { "nodes": [] }
        } } } }"#;
        let groups = parse_and_group_review_feedback(json).expect("parse ok");
        assert_eq!(groups.len(), 1);
        // The reply still threads onto the original comment.
        assert_eq!(groups[0].reply_comment_id, Some(1));
        // All three comments reach the planner, in order and attributed, so it
        // can see its own prior (wrong) reply and the reviewer's correction.
        assert_eq!(groups[0].comments.len(), 3);
        let combined = groups[0].combined_text();
        assert!(combined.contains("@marcos: This function should take 4 parameters"));
        assert!(combined.contains("@me: Addressed in abc123"));
        assert!(combined.contains("@marcos: You changed it to 3 parameters"));
    }

    #[test]
    fn group_review_feedback_keeps_thread_ending_in_non_resolution_viewer_comment() {
        // A viewer comment that is *not* one of our resolution replies (e.g. a
        // human note typed from the same account) must not be mistaken for a
        // handled thread — it is kept for analysis.
        let json = r#"{ "data": { "repository": { "pullRequest": {
            "reviewThreads": { "nodes": [
              { "isResolved": false, "comments": { "nodes": [
                { "databaseId": 1, "path": "a.rs", "line": 4, "isMinimized": false, "viewerDidAuthor": false, "body": "rename this", "author": { "login": "marcos" } },
                { "databaseId": 2, "path": "a.rs", "line": 4, "isMinimized": false, "viewerDidAuthor": true, "body": "good point, let me think about it", "author": { "login": "me" } }
              ] } }
            ] },
            "reviews": { "nodes": [] }
        } } } }"#;
        let groups = parse_and_group_review_feedback(json).expect("parse ok");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].comments.len(), 2);
    }

    #[test]
    fn group_review_feedback_skips_thread_ending_in_no_change_reply() {
        // The "already addresses this — no change needed" reply is also a
        // resolution reply, so a thread ending in it is dropped.
        let json = r#"{ "data": { "repository": { "pullRequest": {
            "reviewThreads": { "nodes": [
              { "isResolved": false, "comments": { "nodes": [
                { "databaseId": 1, "path": "a.rs", "line": 4, "isMinimized": false, "viewerDidAuthor": false, "body": "use a constant", "author": { "login": "marcos" } },
                { "databaseId": 2, "path": "a.rs", "line": 4, "isMinimized": false, "viewerDidAuthor": true, "body": "NO_CHANGE_REPLY", "author": { "login": "me" } }
              ] } }
            ] },
            "reviews": { "nodes": [] }
        } } } }"#
        .replace("NO_CHANGE_REPLY", ALREADY_RESOLVED_REPLY);
        let groups = parse_and_group_review_feedback(&json).expect("parse ok");
        assert!(groups.is_empty());
    }

    #[test]
    fn group_review_feedback_folds_only_submitted_nonempty_summaries() {
        let json = r#"{ "data": { "repository": { "pullRequest": {
            "reviewThreads": { "nodes": [] },
            "reviews": { "nodes": [
              { "state": "CHANGES_REQUESTED", "body": "Please add tests.", "author": { "login": "alice" } },
              { "state": "PENDING", "body": "draft note, not submitted", "author": { "login": "me" } },
              { "state": "DISMISSED", "body": "old retracted review", "author": { "login": "bob" } },
              { "state": "APPROVED", "body": "", "author": { "login": "ci" } },
              { "state": "COMMENTED", "body": "Also rename the module.", "author": { "login": "carol" } }
            ] }
        } } } }"#;
        let groups = parse_and_group_review_feedback(json).expect("parse ok");
        // One folded group from the two submitted, non-empty summaries; PENDING,
        // DISMISSED, and empty-body reviews are excluded.
        assert_eq!(groups.len(), 1);
        let summary = &groups[0];
        assert!(summary.reply_comment_id.is_none());
        assert_eq!(summary.comments.len(), 2);
        assert_eq!(summary.comments[0].author, "alice");
        assert_eq!(summary.comments[1].author, "carol");
        // The combined text carries both summaries into the single planning call.
        let combined = summary.combined_text();
        assert!(combined.contains("Please add tests."));
        assert!(combined.contains("Also rename the module."));
    }

    #[test]
    fn praise_reaction_targets_reviewers_latest_comment_not_the_change_request() {
        // The thread the user reported: the reviewer first asks for a change,
        // we justify keeping the code, and the reviewer concedes with praise.
        // The 😄 must land on that closing praise (id 3), not the opening
        // change request (id 1, which `reply_comment_id` anchors replies to).
        let group = CommentGroup {
            file: Some("app/helpers/ai_assistant_helper.rb".to_string()),
            line: Some(11),
            reply_comment_id: Some(1),
            comments: vec![
                ReviewComment {
                    author: "marcos".to_string(),
                    body: "essa linha está com a gramatica errada, retirar acento.".to_string(),
                    database_id: Some(1),
                    viewer_did_author: false,
                },
                ReviewComment {
                    author: "victorcorcos".to_string(),
                    body: "Obrigado, mas vou manter \"portfólio\" com acento.".to_string(),
                    database_id: Some(2),
                    viewer_did_author: true,
                },
                ReviewComment {
                    author: "marcos".to_string(),
                    body: "é... realmente você tem razão! parabéns.".to_string(),
                    database_id: Some(3),
                    viewer_did_author: false,
                },
            ],
        };
        assert_eq!(group.praise_reaction_target_id(), Some(3));
    }

    #[test]
    fn praise_reaction_skips_our_own_trailing_comment() {
        // If our own comment is the most recent, the reaction still targets the
        // reviewer's last comment — we never react to ourselves.
        let group = CommentGroup {
            file: Some("a.rs".to_string()),
            line: Some(4),
            reply_comment_id: Some(1),
            comments: vec![
                ReviewComment {
                    author: "marcos".to_string(),
                    body: "great job here!".to_string(),
                    database_id: Some(1),
                    viewer_did_author: false,
                },
                ReviewComment {
                    author: "victorcorcos".to_string(),
                    body: "thanks!".to_string(),
                    database_id: Some(2),
                    viewer_did_author: true,
                },
            ],
        };
        assert_eq!(group.praise_reaction_target_id(), Some(1));
    }

    #[test]
    fn praise_reaction_none_for_pr_level_summary() {
        // PR-level summaries have no inline-comment anchor, so there is nothing
        // to react to and the courtesy is skipped.
        let group = CommentGroup {
            file: None,
            line: None,
            reply_comment_id: None,
            comments: vec![ReviewComment {
                author: "carol".to_string(),
                body: "Looks good overall".to_string(),
                database_id: None,
                viewer_did_author: false,
            }],
        };
        assert_eq!(group.praise_reaction_target_id(), None);
    }

    #[test]
    fn group_review_feedback_retains_per_comment_ids_and_authorship() {
        // End-to-end through the parser: the praise reaction target resolves to
        // the reviewer's closing comment, not the opening change request.
        let json = r#"{ "data": { "repository": { "pullRequest": {
            "reviewThreads": { "nodes": [
              { "isResolved": false, "comments": { "nodes": [
                { "databaseId": 1, "path": "a.rb", "line": 11, "isMinimized": false, "viewerDidAuthor": false, "body": "retirar acento", "author": { "login": "marcos" } },
                { "databaseId": 2, "path": "a.rb", "line": 11, "isMinimized": false, "viewerDidAuthor": true, "body": "vou manter o acento", "author": { "login": "me" } },
                { "databaseId": 3, "path": "a.rb", "line": 11, "isMinimized": false, "viewerDidAuthor": false, "body": "você tem razão! parabéns", "author": { "login": "marcos" } }
              ] } }
            ] },
            "reviews": { "nodes": [] }
        } } } }"#;
        let groups = parse_and_group_review_feedback(json).expect("parse ok");
        assert_eq!(groups.len(), 1);
        // Replies still thread onto the original comment…
        assert_eq!(groups[0].reply_comment_id, Some(1));
        // …but the praise reaction targets the reviewer's latest comment.
        assert_eq!(groups[0].praise_reaction_target_id(), Some(3));
        assert!(groups[0].comments[1].viewer_did_author);
    }

    #[test]
    fn group_review_feedback_empty_when_all_resolved() {
        let json = r#"{ "data": { "repository": { "pullRequest": {
            "reviewThreads": { "nodes": [
              { "isResolved": true, "isOutdated": false, "comments": { "nodes": [
                { "databaseId": 1, "path": "a.rs", "line": 5, "isMinimized": false, "viewerDidAuthor": false, "body": "x", "author": { "login": "rev" } }
              ] } }
            ] },
            "reviews": { "nodes": [] }
        } } } }"#;
        assert!(parse_and_group_review_feedback(json).unwrap().is_empty());
    }

    #[test]
    fn group_review_feedback_surfaces_graphql_errors() {
        let json = r#"{ "errors": [ { "message": "Could not resolve to a Repository." } ] }"#;
        assert!(parse_and_group_review_feedback(json).is_err());
    }

    // ── Fix pipeline: commit + reply formatting ────────────────────────

    fn sample_plan() -> FixPlan {
        FixPlan {
            summary: "extract retry delay into a named constant".to_string(),
            validity: "Valid.".to_string(),
            explanation: "Replace the literal 3000 with RETRY_DELAY_MS.".to_string(),
            change: "diff".to_string(),
        }
    }

    #[test]
    fn format_commit_message_follows_review_format() {
        let group = CommentGroup {
            file: Some("src/retry.rs".to_string()),
            line: Some(12),
            reply_comment_id: Some(7),
            comments: vec![ReviewComment {
                author: "alice".to_string(),
                body: "Magic number 3000 is unclear".to_string(),
                database_id: Some(7),
                viewer_did_author: false,
            }],
        };
        let (subject, body) = format_commit_message(42, 2, &group.brief(), &sample_plan());
        assert_eq!(
            subject,
            "fix (review): extract retry delay into a named constant"
        );
        assert!(body.starts_with("PR #42, comment #2 — \"Magic number 3000 is unclear\""));
        assert!(body.contains("RETRY_DELAY_MS"));
    }

    #[test]
    fn format_commit_message_defaults_blank_summary() {
        let plan = FixPlan {
            summary: "   ".to_string(),
            validity: String::new(),
            explanation: String::new(),
            change: String::new(),
        };
        let (subject, body) = format_commit_message(1, 1, "feedback", &plan);
        assert_eq!(subject, "fix (review): address review comment");
        // Explanation falls back to the (defaulted) summary.
        assert!(body.ends_with("address review comment"));
    }

    #[test]
    fn format_reply_links_the_commit() {
        let reply = format_reply(
            "https://github.com/o/r/pull/42/changes/abc123",
            &sample_plan(),
        );
        assert_eq!(
            reply,
            "Addressed in https://github.com/o/r/pull/42/changes/abc123 — \
             extract retry delay into a named constant. Thanks for the feedback!"
        );
    }

    #[test]
    fn is_resolution_reply_matches_our_replies_only() {
        // Both replies the Fix loop posts to mark a thread handled.
        assert!(is_resolution_reply(&format_reply(
            "https://x/commit/abc",
            &sample_plan()
        )));
        assert!(is_resolution_reply(ALREADY_RESOLVED_REPLY));
        // A reviewer follow-up or an arbitrary viewer note is not a resolution.
        assert!(!is_resolution_reply("You changed the wrong function!"));
        assert!(!is_resolution_reply("good point, let me think about it"));
    }

    // ── Fix pipeline: prompt substitution ──────────────────────────────

    const PLAN_TOKENS: [&str; 7] = [
        "FILE_PATH",
        "COMMENT_LINES",
        "REVIEW_COMMENTS",
        "CODE_CONTEXT",
        "USER_FEEDBACK",
        "PREVIOUS_PLAN",
        "PROCESSED_HISTORY",
    ];

    #[test]
    fn build_fix_plan_prompt_substitutes_inputs_and_leaks_no_tokens() {
        let group = CommentGroup {
            file: Some("src/retry.rs".to_string()),
            line: Some(12),
            reply_comment_id: Some(7),
            comments: vec![ReviewComment {
                author: "alice".to_string(),
                body: "Magic number 3000 is unclear".to_string(),
                database_id: Some(7),
                viewer_did_author: false,
            }],
        };
        let prompt = build_fix_plan_prompt(
            &group,
            "   12 | sleep(3000)\n",
            Some("avoid nested ifs"),
            Some("old plan text"),
            Some("[#1] src/styles.css:20 — Applied a fix: change the color to purple"),
        );
        assert!(prompt.contains("src/retry.rs"));
        assert!(prompt.contains("Magic number 3000 is unclear"));
        assert!(prompt.contains("sleep(3000)"));
        assert!(prompt.contains("avoid nested ifs"));
        assert!(prompt.contains("old plan text"));
        assert!(prompt.contains("change the color to purple"));
        for token in PLAN_TOKENS {
            assert!(!prompt.contains(token), "leaked placeholder token: {token}");
        }
    }

    #[test]
    fn build_fix_plan_prompt_defaults_optional_inputs_for_pr_level() {
        let group = CommentGroup {
            file: None,
            line: None,
            reply_comment_id: None,
            comments: vec![ReviewComment {
                author: "carol".to_string(),
                body: "Looks good overall".to_string(),
                database_id: None,
                viewer_did_author: false,
            }],
        };
        let prompt = build_fix_plan_prompt(&group, "", None, None, None);
        assert!(prompt.contains("(none)")); // feedback + previous plan defaults
        assert!(prompt.contains("(no code context"));
        assert!(prompt.contains("(none — this is the first comment")); // history default
        for token in PLAN_TOKENS {
            assert!(!prompt.contains(token), "leaked placeholder token: {token}");
        }
    }

    #[test]
    fn build_fix_apply_prompt_substitutes_inputs_and_leaks_no_tokens() {
        let group = CommentGroup {
            file: Some("src/retry.rs".to_string()),
            line: Some(12),
            reply_comment_id: Some(7),
            comments: vec![ReviewComment {
                author: "alice".to_string(),
                body: "rename foo to bar".to_string(),
                database_id: Some(7),
                viewer_did_author: false,
            }],
        };
        let prompt = build_fix_apply_prompt(&group, &sample_plan());
        assert!(prompt.contains("src/retry.rs"));
        assert!(prompt.contains("rename foo to bar"));
        assert!(prompt.contains("extract retry delay into a named constant"));
        for token in ["TARGET_FILES", "REVIEW_COMMENT", "APPROVED_PLAN"] {
            assert!(!prompt.contains(token), "leaked placeholder token: {token}");
        }
    }

    fn make_executable(path: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streamed_command_sanitizes_terminal_activity_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("ansi-output.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '\\033[31mred\\033[0m\\rstdout\\n'\nprintf '\\033[2K\\033[35mstderr\\033[0m\\n' >&2\n",
        )
        .unwrap();
        make_executable(&script);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let stdout = run_command_streamed(&script, &[], Some(dir.path()), Some(&tx))
            .await
            .expect("command should succeed");
        drop(tx);

        let mut activity = Vec::new();
        while let Ok((text, kind)) = rx.try_recv() {
            activity.push((text, kind));
        }

        assert_eq!(stdout, "stdout");
        assert_eq!(activity.len(), 2, "activity was {activity:?}");
        assert!(
            activity.contains(&("stdout".to_string(), ActivityKind::Stdout)),
            "stdout entry missing from activity: {activity:?}"
        );
        assert!(
            activity.contains(&("stderr".to_string(), ActivityKind::Stderr)),
            "stderr entry missing from activity: {activity:?}"
        );

        for text in activity.iter().map(|(text, _)| text).chain([&stdout]) {
            assert!(
                !text.contains('\x1b'),
                "ANSI escaped into activity: {text:?}"
            );
            assert!(
                !text.chars().any(|c| c.is_control() && c != '\t'),
                "control byte escaped into activity: {text:?}"
            );
        }
    }

    // Hermetic git helper: never depend on the machine's global identity/config.
    fn git(cwd: &Path, args: &[&str]) -> String {
        use std::process::Command;

        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git invocation");
        assert!(
            out.status.success(),
            "git {args:?} failed in {cwd:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
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

    #[tokio::test]
    async fn automatic_repo_pinned_helpers_pass_repo_to_gh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join("gh.log");
        let gh_path = dir.path().join("fake-gh.sh");
        std::fs::write(
            &gh_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{log}\"\nif [ \"$1\" = \"--version\" ]; then\n  exit 0\nfi\nif [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then\n  printf '{{\"title\":\"Subject\",\"body\":\"Body\"}}'\n  exit 0\nfi\nif [ \"$1\" = \"pr\" ] && [ \"$2\" = \"merge\" ]; then\n  exit 0\nfi\nexit 1\n",
                log = log_path.display()
            ),
        )
        .unwrap();
        make_executable(&gh_path);

        let service = DashboardService::new(dir.path().to_path_buf(), DashboardConfig::default())
            .with_gh_binary(gh_path);
        let details = service
            .fetch_pr_details_for_repo(7, "owner/repo")
            .await
            .expect("details");
        assert_eq!(details.title, "Subject");
        service
            .merge_pull_request_in_repo(7, &details.title, &details.body, "owner/repo", "abc123")
            .await
            .expect("merge");

        let log = std::fs::read_to_string(log_path).unwrap();
        assert!(
            log.contains("pr view 7 --json title,body --repo owner/repo"),
            "automatic detail fetch must pin --repo; log was {log:?}"
        );
        assert!(
            log.contains("pr merge 7 --squash --subject Subject (#7) --body Body --repo owner/repo --match-head-commit abc123"),
            "automatic merge must pin --repo and --match-head-commit; log was {log:?}"
        );
    }

    #[tokio::test]
    async fn fetch_base_ref_advances_stale_remote_tracking_ref() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = tmp.path().join("remote.git");
        let work = tmp.path().join("work");
        let local = tmp.path().join("local");
        std::fs::create_dir_all(&work).unwrap();
        let remote_str = remote.to_str().unwrap();

        // Bare remote (the shared base), with `main` as its default branch.
        git(tmp.path(), &["init", "-q", "--bare", remote_str]);
        git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        // A working clone seeds `main` and pushes it to the remote.
        git(&work, &["init", "-q"]);
        git(&work, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        std::fs::write(work.join("a.txt"), "1").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "init"]);
        git(&work, &["remote", "add", "origin", remote_str]);
        git(&work, &["push", "-q", "origin", "main"]);

        // The repo the dashboard polls: a fresh clone, so origin/main matches
        // the seed commit.
        git(
            tmp.path(),
            &["clone", "-q", remote_str, local.to_str().unwrap()],
        );
        let stale = git(&local, &["rev-parse", "origin/main"]);

        // Another developer pushes a new commit to the base branch.
        std::fs::write(work.join("b.txt"), "2").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "second"]);
        git(&work, &["push", "-q", "origin", "main"]);
        let advanced = git(&work, &["rev-parse", "HEAD"]);

        // The dashboard's clone hasn't fetched yet, so its origin/main is stale.
        assert_ne!(stale, advanced);
        assert_eq!(git(&local, &["rev-parse", "origin/main"]), stale);

        // fetch_base_ref refreshes only the base's remote-tracking ref.
        let service = DashboardService::new(local.clone(), DashboardConfig::default());
        let did_advance = service.fetch_base_ref().await;

        assert!(
            did_advance,
            "fetch_base_ref should report that the base ref advanced"
        );
        assert_eq!(
            git(&local, &["rev-parse", "origin/main"]),
            advanced,
            "fetch_base_ref should advance origin/main to the newly pushed commit"
        );
    }

    #[tokio::test]
    async fn local_pr_diff_reproduces_three_dot_diff_from_synced_branch() {
        // The fallback used when GitHub's diff endpoint 5xx's on an oversized
        // PR: compute the same three-dot diff locally. Set up a base branch, a
        // feature branch checked out in the worktree, and confirm the fallback
        // produces a unified diff our parser turns into commentable findings.
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = tmp.path().join("remote.git");
        let work = tmp.path().join("work");
        let local = tmp.path().join("local");
        std::fs::create_dir_all(&work).unwrap();
        let remote_str = remote.to_str().unwrap();

        git(tmp.path(), &["init", "-q", "--bare", remote_str]);
        git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        // Seed `main` with a multi-line file and push it to the remote.
        git(&work, &["init", "-q"]);
        git(&work, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        std::fs::write(work.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "init"]);
        git(&work, &["remote", "add", "origin", remote_str]);
        git(&work, &["push", "-q", "origin", "main"]);

        // The reviewer's worktree: a clone with a feature branch checked out
        // (as it would be after `git pull --ff-only` synced the PR head).
        git(
            tmp.path(),
            &["clone", "-q", remote_str, local.to_str().unwrap()],
        );
        git(&local, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(local.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
        git(&local, &["add", "."]);
        git(&local, &["commit", "-q", "-m", "edit"]);

        let service = DashboardService::new(local.clone(), DashboardConfig::default());
        let diff = service
            .local_pr_diff(&local, "main")
            .await
            .expect("local diff fallback should succeed");

        assert!(
            diff.contains("diff --git a/a.txt b/a.txt"),
            "expected a unified diff header, got: {diff}"
        );
        assert!(diff.contains("+TWO"), "diff should show the edit: {diff}");
        assert!(
            diff.contains("+four"),
            "diff should show the addition: {diff}"
        );

        // The parser turns it into a reviewable file with new-side line
        // numbers — the same anchors GitHub accepts inline comments on.
        let files = parse_review_diff(&diff);
        assert_eq!(files.len(), 1, "one changed file expected: {files:?}");
        assert_eq!(files[0].path, "a.txt");
        assert!(files[0].commentable_lines.contains(&2)); // the changed "TWO"
        assert!(files[0].commentable_lines.contains(&4)); // the added "four"
    }

    #[tokio::test]
    async fn local_pr_diff_reports_error_when_base_ref_unresolvable() {
        // No remote-tracking base ref exists, so the fallback can't reproduce
        // the diff — it must surface a clear error rather than an empty diff.
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["symbolic-ref", "HEAD", "refs/heads/feature"]);
        std::fs::write(repo.join("a.txt"), "x\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);

        let service = DashboardService::new(repo.clone(), DashboardConfig::default());
        let err = service
            .local_pr_diff(&repo, "main")
            .await
            .expect_err("no base ref means no fallback diff");
        assert!(
            err.contains("could not resolve a local base ref"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn fetch_base_ref_reports_no_change_when_base_is_current() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = tmp.path().join("remote.git");
        let work = tmp.path().join("work");
        let local = tmp.path().join("local");
        std::fs::create_dir_all(&work).unwrap();
        let remote_str = remote.to_str().unwrap();

        // Bare remote (the shared base), with `main` as its default branch.
        git(tmp.path(), &["init", "-q", "--bare", remote_str]);
        git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        // A working clone seeds `main` and pushes it to the remote.
        git(&work, &["init", "-q"]);
        git(&work, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        std::fs::write(work.join("a.txt"), "1").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "init"]);
        git(&work, &["remote", "add", "origin", remote_str]);
        git(&work, &["push", "-q", "origin", "main"]);

        // The dashboard's clone is current with the base — nobody pushed since.
        git(
            tmp.path(),
            &["clone", "-q", remote_str, local.to_str().unwrap()],
        );

        let service = DashboardService::new(local.clone(), DashboardConfig::default());
        let did_advance = service.fetch_base_ref().await;

        assert!(
            !did_advance,
            "fetch_base_ref should report no change when the base ref is already current"
        );
    }

    #[tokio::test]
    async fn unpushed_commit_count_tracks_local_commits_until_pushed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = tmp.path().join("remote.git");
        let work = tmp.path().join("work");
        let remote_str = remote.to_str().unwrap();
        std::fs::create_dir_all(&work).unwrap();

        // Bare remote with a `main` clone that is fully pushed to start.
        git(tmp.path(), &["init", "-q", "--bare", remote_str]);
        git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(&work, &["init", "-q"]);
        git(&work, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        std::fs::write(work.join("a.txt"), "1").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "init"]);
        git(&work, &["remote", "add", "origin", remote_str]);
        git(&work, &["push", "-q", "-u", "origin", "main"]);

        let service = DashboardService::new(work.clone(), DashboardConfig::default());
        let work_str = work.to_str().unwrap();

        // Fully pushed → nothing to warn about.
        assert_eq!(service.unpushed_commit_count(work_str).await, 0);

        // Two local commits that never reached the remote.
        std::fs::write(work.join("b.txt"), "2").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "second"]);
        std::fs::write(work.join("c.txt"), "3").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "third"]);
        assert_eq!(service.unpushed_commit_count(work_str).await, 2);

        // Pushing HEAD flushes them, so the count drops back to zero.
        service
            .push_head_to_origin(work_str)
            .await
            .expect("push HEAD");
        assert_eq!(service.unpushed_commit_count(work_str).await, 0);
    }

    #[tokio::test]
    async fn dropping_watch_aborts_tracked_wise_merge_tasks() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().expect("tempdir");
        let service = DashboardService::new(dir.path().to_path_buf(), DashboardConfig::default())
            .with_cache_path(None);
        let watch = service.watch();
        let completed = Arc::new(AtomicBool::new(false));
        let completed_in_task = completed.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            completed_in_task.store(true, Ordering::SeqCst);
        });
        service
            .wise_merge_tasks
            .lock()
            .expect("wise_merge_tasks poisoned")
            .push(task);

        drop(watch);
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(
            !completed.load(Ordering::SeqCst),
            "tracked Wise Merge task should be aborted before it completes"
        );
        assert!(
            service
                .wise_merge_tasks
                .lock()
                .expect("wise_merge_tasks poisoned")
                .is_empty(),
            "dropping the watch should drain tracked Wise Merge handles"
        );
    }

    #[test]
    fn wise_merge_does_not_shorten_github_pr_refresh_period() {
        let config = DashboardConfig {
            wise_merge: true,
            ..DashboardConfig::default()
        };
        assert_eq!(
            pr_refresh_period(&config),
            Duration::from_millis(PR_REFRESH_PERIOD_MS)
        );
    }

    #[test]
    fn finish_wise_merge_marks_cached_pr_as_merged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = DashboardService::new(dir.path().to_path_buf(), DashboardConfig::default())
            .with_cache_path(None);
        let row = wise_merge_row(Some(ReviewStatus::Approved));
        let pr = row.pull_request.expect("pull request");
        {
            let mut state = service.pr_state.lock().expect("pr_state poisoned");
            state.entries.insert(
                "feature".to_string(),
                PrCacheEntry {
                    sha: "abc123".to_string(),
                    pull_request: Some(pr),
                },
            );
        }

        service.finish_wise_merge(42, Ok("upstream/main".to_string()));

        let state = service.pr_state.lock().expect("pr_state poisoned");
        let pr = state
            .entries
            .get("feature")
            .and_then(|entry| entry.pull_request.as_ref())
            .expect("cached pull request");
        assert_eq!(pr.state, PrState::Merged);
        assert!(state.dirty);
    }

    #[test]
    fn parses_pr_repo_from_view_url() {
        // gh resolves the base repo even in a fork-and-PR-to-upstream setup;
        // the slug must come from the PR url (the repo it was opened against),
        // not the fork the branch is pushed to.
        let raw = r#"{"url":"https://github.com/oxeanbits/digitalize-front/pull/4420"}"#;
        assert_eq!(
            parse_pr_repo_json(raw),
            Some(("oxeanbits".into(), "digitalize-front".into()))
        );
    }

    #[test]
    fn parse_pr_repo_json_rejects_invalid_json() {
        assert_eq!(parse_pr_repo_json("not json at all"), None);
    }

    #[test]
    fn parse_pr_repo_json_rejects_missing_url() {
        assert_eq!(parse_pr_repo_json(r#"{"number":4420}"#), None);
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
    fn detects_transient_gh_5xx_errors() {
        assert!(is_transient_gh_error(
            "could not find pull request diff: HTTP 503: 503 Service Unavailable \
             (https://api.github.com/repos/oxeanbits/digitalize-front/pulls/4651)"
        ));
        assert!(is_transient_gh_error("HTTP 502: Bad Gateway"));
        assert!(!is_transient_gh_error(
            "HTTP 404: Not Found (https://api.github.com/repos/o/r/pulls/1)"
        ));
        assert!(!is_transient_gh_error("network is unreachable"));
    }

    #[tokio::test]
    async fn run_gh_command_retries_transient_5xx_then_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let counter_path = dir.path().join("attempts");
        let gh_path = dir.path().join("fake-gh.sh");
        std::fs::write(
            &gh_path,
            format!(
                "#!/bin/sh\n\
                 n=$(cat \"{counter}\" 2>/dev/null || echo 0)\n\
                 n=$((n + 1))\n\
                 printf '%s' \"$n\" > \"{counter}\"\n\
                 if [ \"$n\" -lt 3 ]; then\n\
                 echo 'HTTP 503: 503 Service Unavailable' >&2\n\
                 exit 1\n\
                 fi\n\
                 printf 'diff --git a/x b/x\\n'\n",
                counter = counter_path.display()
            ),
        )
        .unwrap();
        make_executable(&gh_path);

        let out = run_gh_command(&gh_path, &["pr", "diff", "1"], Some(dir.path()))
            .await
            .expect("should succeed after retries");
        assert_eq!(out, "diff --git a/x b/x");
        assert_eq!(
            std::fs::read_to_string(&counter_path).unwrap(),
            "3",
            "should have retried twice before succeeding on the third attempt"
        );
    }

    #[tokio::test]
    async fn run_gh_command_does_not_retry_permanent_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let counter_path = dir.path().join("attempts");
        let gh_path = dir.path().join("fake-gh.sh");
        std::fs::write(
            &gh_path,
            format!(
                "#!/bin/sh\n\
                 n=$(cat \"{counter}\" 2>/dev/null || echo 0)\n\
                 n=$((n + 1))\n\
                 printf '%s' \"$n\" > \"{counter}\"\n\
                 echo 'HTTP 404: Not Found' >&2\n\
                 exit 1\n",
                counter = counter_path.display()
            ),
        )
        .unwrap();
        make_executable(&gh_path);

        let err = run_gh_command(&gh_path, &["pr", "diff", "1"], Some(dir.path()))
            .await
            .expect_err("permanent failure should surface");
        assert!(err.contains("HTTP 404"), "unexpected error: {err}");
        assert_eq!(
            std::fs::read_to_string(&counter_path).unwrap(),
            "1",
            "a non-transient error must return on the first attempt"
        );
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
        assert!(q.contains("baseRefName"));
        assert!(q.contains("baseRepository"));
        assert!(q.contains("nameWithOwner"));
        assert!(q.contains("headRefOid"));
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
              "b0": {"nodes": [{"number": 7, "state": "OPEN", "url": "u", "title": "t", "isDraft": false, "baseRefName": "main", "baseRepository": {"nameWithOwner": "owner/repo"}, "headRefOid": "abc123"}]},
              "b1": {"nodes": []}
            }
          }
        }"#;
        let out = parse_graphql_response(body, &["feat", "fix"]).unwrap();
        let pr = out.get("feat").unwrap().as_ref().unwrap();
        assert_eq!(pr.number, 7);
        assert_eq!(pr.base_ref_name.as_deref(), Some("main"));
        assert_eq!(pr.base_repository.as_deref(), Some("owner/repo"));
        assert_eq!(pr.head_ref_oid.as_deref(), Some("abc123"));
        assert!(out.get("fix").unwrap().is_none());
    }

    #[test]
    fn parses_closed_draft_pr_as_closed_not_drafted() {
        // GitHub leaves `isDraft = true` on a PR closed while still a draft;
        // the terminal CLOSED state must win so it doesn't read as "Drafted".
        let body = r#"{
          "data": {
            "repository": {
              "b0": {"nodes": [{"number": 9, "state": "CLOSED", "url": "u", "title": "t", "isDraft": true}]}
            }
          }
        }"#;
        let out = parse_graphql_response(body, &["feat"]).unwrap();
        assert_eq!(
            out.get("feat").unwrap().as_ref().unwrap().state,
            PrState::Closed
        );
    }

    #[test]
    fn parses_merged_draft_pr_as_merged_not_drafted() {
        // A draft can be merged via admin merge; MERGED still outranks the flag.
        let body = r#"{
          "data": {
            "repository": {
              "b0": {"nodes": [{"number": 9, "state": "MERGED", "url": "u", "title": "t", "isDraft": true}]}
            }
          }
        }"#;
        let out = parse_graphql_response(body, &["feat"]).unwrap();
        assert_eq!(
            out.get("feat").unwrap().as_ref().unwrap().state,
            PrState::Merged
        );
    }

    #[test]
    fn parses_open_draft_pr_as_drafted() {
        let body = r#"{
          "data": {
            "repository": {
              "b0": {"nodes": [{"number": 9, "state": "OPEN", "url": "u", "title": "t", "isDraft": true}]}
            }
          }
        }"#;
        let out = parse_graphql_response(body, &["feat"]).unwrap();
        assert_eq!(
            out.get("feat").unwrap().as_ref().unwrap().state,
            PrState::Draft
        );
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

    fn wise_merge_row(review_status: Option<ReviewStatus>) -> DashboardRow {
        DashboardRow {
            worktree: GitWorktree {
                path: "/tmp/repo-feature".to_string(),
                branch: "feature".to_string(),
                commit: "abc123".to_string(),
                is_main: false,
                is_clean: true,
                branch_status: None,
            },
            last_commit: None,
            pull_request: Some(PullRequest {
                number: 42,
                state: PrState::Open,
                url: "https://github.com/example/repo/pull/42".to_string(),
                title: "Ready".to_string(),
                base_ref_name: Some("main".to_string()),
                base_repository: Some("example/repo".to_string()),
                head_ref_oid: Some("abc123".to_string()),
                labels: vec![],
                checks_status: Some(CheckStatus::Passed),
                review_status,
                merge_status: Some(MergeStatus::Clean),
                reviewers: ReviewerSummary::default(),
            }),
            ai_status: None,
            error: None,
        }
    }

    #[test]
    fn wise_merge_candidate_accepts_approved_clean_passed_pr() {
        let row = wise_merge_row(Some(ReviewStatus::Approved));
        let candidate = wise_merge_candidate(&row).expect("ready PR should match");
        assert_eq!(candidate.number, 42);
        assert_eq!(candidate.worktree_path, "/tmp/repo-feature");
        assert_eq!(candidate.base_ref_name, "main");
        assert_eq!(candidate.base_repository, "example/repo");
        assert_eq!(candidate.head_ref_oid, "abc123");
    }

    #[test]
    fn wise_merge_candidate_accepts_clean_passed_pr_without_review_requirement() {
        let row = wise_merge_row(None);
        assert!(wise_merge_candidate(&row).is_some());
    }

    #[test]
    fn wise_merge_candidate_rejects_pending_reviews_or_unclean_merge_state() {
        let pending = wise_merge_row(Some(ReviewStatus::Pending));
        assert!(wise_merge_candidate(&pending).is_none());

        let mut dirty = wise_merge_row(Some(ReviewStatus::Approved));
        dirty.pull_request.as_mut().unwrap().merge_status = Some(MergeStatus::Dirty);
        assert!(wise_merge_candidate(&dirty).is_none());
    }

    #[test]
    fn wise_merge_candidate_rejects_failed_checks_and_non_open_prs() {
        let mut failed = wise_merge_row(Some(ReviewStatus::Approved));
        failed.pull_request.as_mut().unwrap().checks_status = Some(CheckStatus::Failed);
        assert!(wise_merge_candidate(&failed).is_none());

        let mut merged = wise_merge_row(Some(ReviewStatus::Approved));
        merged.pull_request.as_mut().unwrap().state = PrState::Merged;
        assert!(wise_merge_candidate(&merged).is_none());
    }

    #[test]
    fn wise_merge_candidate_rejects_missing_safety_fields() {
        let mut missing_base = wise_merge_row(Some(ReviewStatus::Approved));
        missing_base.pull_request.as_mut().unwrap().base_ref_name = None;
        assert!(wise_merge_candidate(&missing_base).is_none());

        let mut missing_head = wise_merge_row(Some(ReviewStatus::Approved));
        missing_head.pull_request.as_mut().unwrap().head_ref_oid = None;
        assert!(wise_merge_candidate(&missing_head).is_none());

        let mut missing_repository = wise_merge_row(Some(ReviewStatus::Approved));
        missing_repository
            .pull_request
            .as_mut()
            .unwrap()
            .base_repository = None;
        assert!(wise_merge_candidate(&missing_repository).is_none());
    }

    #[test]
    fn wise_merge_candidate_accepts_ready_pr_when_local_worktree_lags_remote_head() {
        let mut row = wise_merge_row(Some(ReviewStatus::Approved));
        row.pull_request.as_mut().unwrap().head_ref_oid = Some("def456".to_string());
        let candidate = wise_merge_candidate(&row).expect("ready PR should match");
        assert_eq!(candidate.head_ref_oid, "def456");
    }

    #[test]
    fn wise_merge_base_validation_requires_pr_base_to_match_resolved_base_ref_branch() {
        let row = wise_merge_row(Some(ReviewStatus::Approved));
        let candidate = wise_merge_candidate(&row).expect("candidate");
        assert!(validate_wise_merge_base(&candidate, "upstream/main").is_ok());

        let err = validate_wise_merge_base(&candidate, "upstream/develop").unwrap_err();
        assert!(format!("{err}").contains("does not match resolved base ref"));
    }

    #[test]
    fn wise_merge_repository_validation_requires_pr_base_repo_to_match_base_remote_repo() {
        let row = wise_merge_row(Some(ReviewStatus::Approved));
        let candidate = wise_merge_candidate(&row).expect("candidate");
        assert!(validate_wise_merge_repository(&candidate, "example/repo").is_ok());

        let err = validate_wise_merge_repository(&candidate, "other/repo").unwrap_err();
        assert!(format!("{err}").contains("does not match resolved base repository"));
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
        // some CLIs treat the prompt as a skill manifest and package it
        // into a `.skill` archive instead of resolving conflicts.
        let prompt = build_merge_prompt("upstream/main", &["a.rs".to_string()]);
        assert!(
            !prompt.trim_start().starts_with("---"),
            "prompt must not start with YAML frontmatter, got:\n{prompt}"
        );
    }

    #[test]
    fn run_variant_args_emits_flag_only_for_a_set_thinking() {
        // `opencode run` always accepts `--variant`, so the plan phase passes
        // the configured reasoning effort through. Empty (Default) → no flag.
        assert!(run_variant_args("").is_empty());
        assert!(run_variant_args("   ").is_empty());
        assert_eq!(run_variant_args("high"), vec!["--variant", "high"]);
        assert_eq!(run_variant_args("  max "), vec!["--variant", "max"]);
    }

    #[test]
    fn merged_variant_state_seeds_variant_into_empty_state() {
        // No prior model.json → a fresh object carrying just the seeded effort.
        let next = merged_variant_state(None, "openai/gpt-5.4", "high");
        assert_eq!(next["variant"]["openai/gpt-5.4"], serde_json::json!("high"));
    }

    #[test]
    fn merged_variant_state_preserves_other_fields_and_overwrites_same_model() {
        // A realistic model.json: recent/favorite and other models' variants
        // must survive untouched; only the target model's effort changes.
        let current = serde_json::json!({
            "recent": [{ "providerID": "openai", "modelID": "gpt-5.4" }],
            "favorite": [],
            "variant": {
                "openai/gpt-5.5": "low",
                "openai/gpt-5.4": "medium"
            }
        });
        let next = merged_variant_state(Some(current), "openai/gpt-5.4", "high");
        assert_eq!(
            next["recent"],
            serde_json::json!([{ "providerID": "openai", "modelID": "gpt-5.4" }])
        );
        assert_eq!(next["favorite"], serde_json::json!([]));
        // Untouched sibling variant.
        assert_eq!(next["variant"]["openai/gpt-5.5"], serde_json::json!("low"));
        // Target model overwritten medium → high.
        assert_eq!(next["variant"]["openai/gpt-5.4"], serde_json::json!("high"));
    }

    #[test]
    fn merged_variant_state_writes_default_sentinel_for_blank_thinking() {
        // Empty thinking (the persisted "Default") must clear any stale effort
        // by writing opencode's "default" sentinel, not the empty string.
        let current = serde_json::json!({ "variant": { "openai/gpt-5.4": "max" } });
        for blank in ["", "   "] {
            let next = merged_variant_state(Some(current.clone()), "openai/gpt-5.4", blank);
            assert_eq!(
                next["variant"]["openai/gpt-5.4"],
                serde_json::json!("default")
            );
        }
    }

    #[test]
    fn merged_variant_state_recovers_from_a_non_object_variant_field() {
        // A corrupt/unexpected `variant` (here an array) is replaced with a
        // fresh object rather than panicking or dropping the seed.
        let current = serde_json::json!({ "variant": [1, 2, 3] });
        let next = merged_variant_state(Some(current), "openai/gpt-5.4", "high");
        assert_eq!(next["variant"]["openai/gpt-5.4"], serde_json::json!("high"));
    }

    #[test]
    fn seed_opencode_tui_variant_at_round_trips_through_disk() {
        // End-to-end against a tempdir: the seeded effort lands in model.json
        // and a subsequent seed of a different model is merged in, not clobbered.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("opencode").join("model.json");

        seed_opencode_tui_variant_at(&path, "openai/gpt-5.4", "high");
        let first: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read seeded file"))
                .expect("valid json");
        assert_eq!(
            first["variant"]["openai/gpt-5.4"],
            serde_json::json!("high")
        );

        seed_opencode_tui_variant_at(&path, "opencode/glm-5.2", "max");
        let second: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read seeded file"))
                .expect("valid json");
        // Both entries coexist after the second seed.
        assert_eq!(
            second["variant"]["openai/gpt-5.4"],
            serde_json::json!("high")
        );
        assert_eq!(
            second["variant"]["opencode/glm-5.2"],
            serde_json::json!("max")
        );
    }

    #[test]
    fn seed_opencode_tui_variant_at_is_a_noop_for_blank_model() {
        // A blank model id must never create or touch the state file.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("opencode").join("model.json");
        seed_opencode_tui_variant_at(&path, "   ", "high");
        assert!(!path.exists());
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
    fn build_merge_prompt_forbids_unrelated_work_and_pipeline_git_ops() {
        let prompt = build_merge_prompt("upstream/main", &["a.rs".to_string()]);
        // The prompt must tell the AI to stay out of pipeline-managed git ops
        // and not to drift into unrelated cleanup — both are concrete failure
        // modes we've already observed in production.
        assert!(prompt.contains("git commit"));
        assert!(prompt.contains("git push"));
        assert!(prompt.to_lowercase().contains("stay focused on the merge"));
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

    #[test]
    fn extract_ticket_normalizes_acronym_and_hyphen() {
        assert_eq!(
            extract_ticket("digit3131-add-retry"),
            Some("DIGIT-3131".into())
        );
        assert_eq!(extract_ticket("DIGIT-42-fix"), Some("DIGIT-42".into()));
        assert_eq!(
            extract_ticket("feature/dpms-9-thing"),
            Some("DPMS-9".into())
        );
        assert_eq!(extract_ticket("just-a-branch"), None);
    }

    #[test]
    fn parse_pull_request_md_splits_title_and_body() {
        let md = "DIGIT-3131 Add payment retry logic\n\n# Description ✍️\n\nDetails here.";
        let (title, body, labels) = parse_pull_request_md(md).expect("parsed");
        assert_eq!(title, "DIGIT-3131 Add payment retry logic");
        assert!(body.starts_with("# Description ✍️"));
        assert!(body.contains("Details here."));
        assert!(labels.is_empty());
    }

    #[test]
    fn parse_pull_request_md_strips_heading_marker_from_title() {
        let md = "# My title line\n\nbody";
        let (title, body, labels) = parse_pull_request_md(md).expect("parsed");
        assert_eq!(title, "My title line");
        assert_eq!(body, "body");
        assert!(labels.is_empty());
    }

    #[test]
    fn parse_pull_request_md_returns_none_when_empty() {
        assert!(parse_pull_request_md("\n\n   \n").is_none());
    }

    #[test]
    fn parse_pull_request_md_extracts_wisetree_labels() {
        let md = "Fix login crash\n<!-- wisetree-labels: bug 🐛, security 🛡️ -->\n\n# Description\n\nDetails.";
        let (title, body, labels) = parse_pull_request_md(md).expect("parsed");
        assert_eq!(title, "Fix login crash");
        assert!(
            !body.contains("wisetree-labels"),
            "comment should be stripped from body"
        );
        assert_eq!(labels, vec!["bug 🐛", "security 🛡️"]);
    }

    #[test]
    fn parse_pull_request_md_handles_single_label() {
        let md = "Add docs\n<!-- wisetree-labels: documentation 📖 -->\n\n# Description\n\nBody.";
        let (_, _, labels) = parse_pull_request_md(md).expect("parsed");
        assert_eq!(labels, vec!["documentation 📖"]);
    }

    #[test]
    fn build_enrich_prompt_substitutes_all_inputs() {
        let prompt = build_enrich_prompt(
            "upstream/main",
            "digit-3131-retry",
            "DIGIT-3131",
            "### Add retry\n\nbody",
            "diff --git a/x b/x",
            "# Description ✍️",
        );
        assert!(!prompt.contains("BASE_REF"));
        assert!(!prompt.contains("GIT_DIFF"));
        assert!(!prompt.contains("GIT_LOG"));
        assert!(!prompt.contains("PR_TEMPLATE"));
        assert!(prompt.contains("upstream/main"));
        assert!(prompt.contains("DIGIT-3131"));
        assert!(prompt.contains("diff --git a/x b/x"));
    }

    #[test]
    fn truncate_for_prompt_caps_oversized_diff() {
        let big = "x".repeat(10);
        let out = truncate_for_prompt(&big, 4);
        assert!(out.starts_with("xxxx"));
        assert!(out.contains("truncated"));
        let small = "tiny";
        assert_eq!(truncate_for_prompt(small, 100), "tiny");
    }

    #[test]
    fn preserve_media_reinserts_under_overview() {
        let old = "# Description\n\nstuff\n\n# Overview\n\n![shot](https://user-images.githubusercontent.com/1/a.png)\n";
        let new = "# Description ✍️\n\nNew description\n\n# Overview 🔍\n\nplaceholder\n\n# Test Guidance\n\nsteps";
        let merged = preserve_media(old, new);
        assert!(merged.contains("![shot](https://user-images.githubusercontent.com/1/a.png)"));
        let overview_idx = merged.find("# Overview").unwrap();
        let media_idx = merged.find("![shot]").unwrap();
        let guidance_idx = merged.find("# Test Guidance").unwrap();
        // Media lands after the Overview heading but before Test Guidance.
        assert!(overview_idx < media_idx && media_idx < guidance_idx);
    }

    #[test]
    fn preserve_media_no_media_returns_body_unchanged() {
        let old = "# Overview\n\njust text, no media";
        let new = "# Overview 🔍\n\nfresh";
        assert_eq!(preserve_media(old, new), new);
    }

    #[test]
    fn preserve_media_skips_media_already_present() {
        let token = "![s](https://github.com/o/r/assets/1/2)";
        let old = format!("# Overview\n\n{token}");
        let new = format!("# Overview 🔍\n\n{token}");
        // Already present in the new body → not duplicated.
        assert_eq!(preserve_media(&old, &new), new);
    }

    #[test]
    fn pr_number_from_url_parses_pull_path() {
        assert_eq!(
            pr_number_from_url("https://github.com/o/r/pull/321"),
            Some(321)
        );
        assert_eq!(pr_number_from_url("https://github.com/o/r"), None);
    }

    #[test]
    fn pr_url_from_output_finds_last_url_line() {
        let out = "Warning: 3 uncommitted changes\nhttps://github.com/o/r/pull/9";
        assert_eq!(pr_url_from_output(out), "https://github.com/o/r/pull/9");
    }

    // ── Develop: check command + section commit ─────────────────────────

    /// A config whose only non-default is the Develop check command.
    fn develop_config(check_command: &str) -> DashboardConfig {
        let mut config = DashboardConfig::default();
        config.develop.check_command = check_command.to_string();
        config
    }

    /// A committed git repo with identity set, ready for section commits.
    fn develop_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("work");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.name", "t"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        std::fs::write(repo.join("seed.txt"), "seed").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
        (tmp, repo)
    }

    /// A temp repo with an initial commit, ready for failure-path tests.
    fn initialized_temp_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        git(repo, &["init", "-q"]);
        git(repo, &["config", "user.name", "t"]);
        git(repo, &["config", "user.email", "t@example.com"]);
        std::fs::write(repo.join("seed.txt"), "seed").unwrap();
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", "seed"]);
        tmp
    }

    fn initialized_temp_repo_with_change() -> tempfile::TempDir {
        let tmp = initialized_temp_repo();
        std::fs::write(tmp.path().join("seed.txt"), "changed").unwrap();
        tmp
    }

    /// Wrap the real git binary with a script that fails on a specific command.
    fn dashboard_with_failing_git(
        repo: &tempfile::TempDir,
        trigger: &str,
        stderr: &str,
    ) -> DashboardService {
        let parent = repo.path().parent().expect("tempdir parent");
        let name = repo
            .path()
            .file_name()
            .expect("tempdir name")
            .to_string_lossy();
        let wrapper = parent.join(format!("{name}-git"));
        let script = format!(
            "#!/bin/sh\n\
             case \"$*\" in\n\
             \"{trigger}\"|\"{trigger} \"*)\n\
             if [ -n \"{stderr}\" ]; then\n\
             printf '%s\\n' \"{stderr}\" >&2\n\
             fi\n\
             exit 1\n\
             ;;\n\
             esac\n\
             exec git \"$@\"\n"
        );
        std::fs::write(&wrapper, script).expect("write wrapper");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
                .expect("chmod wrapper");
        }
        DashboardService::new(repo.path().to_path_buf(), DashboardConfig::default())
            .with_git_binary(wrapper)
    }

    fn develop_dashboard_service() -> (DashboardService, tempfile::TempDir) {
        let worktree = tempfile::tempdir().expect("tempdir");
        let mut config = DashboardConfig::default();
        config.ai.develop.plan.model = " test-model ".to_string();
        let service = DashboardService::new(worktree.path().to_path_buf(), config)
            .with_opencode_binary(PathBuf::from("git"));
        (service, worktree)
    }

    #[test]
    fn prepare_develop_plan_builds_plan_agent_handoff() {
        let (service, worktree) = develop_dashboard_service();

        let handoff = service
            .prepare_develop_plan(
                worktree.path().to_str().unwrap(),
                "Add dashboard filtering",
                Some("origin/main"),
                None,
                None,
                false,
            )
            .unwrap();

        assert_eq!(handoff.opencode_binary, service.opencode_binary);
        assert_eq!(handoff.cwd, worktree.path());
        assert_eq!(
            handoff.opencode_args[2..6],
            ["-m", "test-model", "--agent", "plan"]
        );
        assert!(handoff
            .opencode_args
            .get(1)
            .unwrap()
            .contains("Add dashboard filtering"));
        assert!(handoff
            .opencode_args
            .get(1)
            .unwrap()
            .contains("origin/main"));
        assert!(!handoff
            .opencode_args
            .get(1)
            .unwrap()
            .contains("Your previous output could not be parsed"));
    }

    #[test]
    fn prepare_develop_plan_appends_corrective_retry_instruction() {
        let (service, worktree) = develop_dashboard_service();

        let handoff = service
            .prepare_develop_plan(
                worktree.path().to_str().unwrap(),
                "Add dashboard filtering",
                None,
                None,
                None,
                true,
            )
            .unwrap();

        assert!(handoff.opencode_args.get(1).unwrap().ends_with(
            "Your previous output could not be parsed. Reply with ONLY the \
             delimited blocks, exactly as specified."
        ));
    }

    #[test]
    fn prepare_develop_plan_rejects_missing_model() {
        let (mut service, worktree) = develop_dashboard_service();
        service.config.ai.develop.plan.model = "   ".to_string();

        let error = service
            .prepare_develop_plan(
                worktree.path().to_str().unwrap(),
                "Add dashboard filtering",
                None,
                None,
                None,
                false,
            )
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "ai.develop.plan model is not configured."
        );
    }

    #[test]
    fn prepare_develop_plan_rejects_unavailable_opencode_binary() {
        let (mut service, worktree) = develop_dashboard_service();
        service.opencode_binary = worktree.path().join("missing-opencode");

        let error = service
            .prepare_develop_plan(
                worktree.path().to_str().unwrap(),
                "Add dashboard filtering",
                None,
                None,
                None,
                false,
            )
            .unwrap_err();

        assert_eq!(error.to_string(), "opencode CLI is not on PATH.");
    }

    #[test]
    fn prepare_develop_implement_builds_expected_handoff() {
        let (mut service, worktree) = develop_dashboard_service();
        service.config.ai.develop.implement.model = " provider/model ".to_string();
        service.config.develop.check_command = "cargo test --all".to_string();

        let handoff = service
            .prepare_develop_implement(
                worktree.path().to_str().unwrap(),
                "Implement task",
                "SECTION content",
                "1. Section [pending]",
                Some("previous check failure"),
            )
            .unwrap();

        assert_eq!(handoff.opencode_binary, service.opencode_binary);
        assert_eq!(handoff.cwd, worktree.path());
        assert_eq!(
            handoff.opencode_args[handoff
                .opencode_args
                .iter()
                .position(|arg| arg == "-m")
                .unwrap()
                + 1],
            "provider/model"
        );
        let prompt = &handoff.opencode_args[handoff
            .opencode_args
            .iter()
            .position(|arg| arg == "--prompt")
            .unwrap()
            + 1];
        assert!(prompt.contains("Implement task"));
        assert!(prompt.contains("SECTION content"));
        assert!(prompt.contains("1. Section [pending]"));
        assert!(prompt.contains("previous check failure"));
    }

    #[test]
    fn prepare_develop_implement_rejects_missing_model() {
        let (mut service, worktree) = develop_dashboard_service();
        service.config.ai.develop.implement.model = "   ".to_string();

        let error = service
            .prepare_develop_implement(
                worktree.path().to_str().unwrap(),
                "task",
                "sections",
                "outline",
                None,
            )
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "ai.develop.implement model is not configured."
        );
    }

    #[test]
    fn prepare_develop_implement_rejects_unavailable_opencode_binary() {
        let (mut service, worktree) = develop_dashboard_service();
        service.config.ai.develop.implement.model = "provider/model".to_string();
        service.opencode_binary = worktree.path().join("missing-opencode");

        let error = service
            .prepare_develop_implement(
                worktree.path().to_str().unwrap(),
                "task",
                "sections",
                "outline",
                None,
            )
            .unwrap_err();

        assert_eq!(error.to_string(), "opencode CLI is not on PATH.");
    }

    #[tokio::test]
    async fn develop_preflight_reports_ai_not_configured_for_blank_planning_model() {
        let (_tmp, repo) = develop_repo();
        let mut config = DashboardConfig::default();
        config.ai.develop.plan.model = " \t ".to_string();
        let service = DashboardService::new(repo.clone(), config);

        let outcome = service
            .develop_preflight(repo.to_str().unwrap())
            .await
            .unwrap();

        assert!(matches!(outcome, DevelopPreflightOutcome::AiNotConfigured));
    }

    #[tokio::test]
    async fn develop_preflight_reports_ai_not_configured_for_blank_implementation_model() {
        let (_tmp, repo) = develop_repo();
        let mut config = DashboardConfig::default();
        config.ai.develop.implement.model = " \t ".to_string();
        let service = DashboardService::new(repo.clone(), config);

        let outcome = service
            .develop_preflight(repo.to_str().unwrap())
            .await
            .unwrap();

        assert!(matches!(outcome, DevelopPreflightOutcome::AiNotConfigured));
    }

    #[tokio::test]
    async fn develop_preflight_reports_ai_unavailable_when_opencode_is_missing() {
        let (_tmp, repo) = develop_repo();
        let service = DashboardService::new(repo.clone(), DashboardConfig::default())
            .with_opencode_binary(repo.join("missing-opencode"));

        let outcome = service
            .develop_preflight(repo.to_str().unwrap())
            .await
            .unwrap();

        assert!(matches!(outcome, DevelopPreflightOutcome::AiUnavailable));
    }

    #[tokio::test]
    async fn develop_preflight_is_ready_with_an_absent_plan() {
        let (_tmp, repo) = develop_repo();
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let expected_base_ref = Some("origin/main".to_string());
        let service = DashboardService::new(repo.clone(), DashboardConfig::default())
            .with_opencode_binary(PathBuf::from("git"));

        let outcome = service
            .develop_preflight(repo.to_str().unwrap())
            .await
            .unwrap();

        let DevelopPreflightOutcome::Ready(preflight) = outcome else {
            panic!("expected Develop preflight to be ready");
        };
        assert!(matches!(preflight.resume, DevelopResumeState::Absent));
        assert_eq!(preflight.base_ref, expected_base_ref);
    }

    #[tokio::test]
    async fn develop_preflight_is_ready_with_an_unparseable_plan() {
        let (_tmp, repo) = develop_repo();
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        std::fs::write(repo.join(PLAN_FILE), "# Not a development plan").unwrap();
        git(&repo, &["add", PLAN_FILE]);
        git(&repo, &["commit", "-q", "-m", "add malformed plan"]);
        let expected_base_ref = Some("origin/main".to_string());
        let service = DashboardService::new(repo.clone(), DashboardConfig::default())
            .with_opencode_binary(PathBuf::from("git"));

        let outcome = service
            .develop_preflight(repo.to_str().unwrap())
            .await
            .unwrap();

        let DevelopPreflightOutcome::Ready(preflight) = outcome else {
            panic!("expected Develop preflight to be ready");
        };
        assert!(matches!(preflight.resume, DevelopResumeState::Unparseable));
        assert_eq!(preflight.base_ref, expected_base_ref);
    }

    #[tokio::test]
    async fn develop_preflight_is_ready_with_a_parsed_plan() {
        let (_tmp, repo) = develop_repo();
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let expected_plan = DevelopPlan {
            task_description: "Add Develop preflight coverage".to_string(),
            complexity: 3,
            sections: vec![PlanSection {
                number: 1,
                name: "Preflight tests".to_string(),
                body: "**Goal**: Cover resume states\n**Acceptance criteria**:\n- [ ] Tests pass"
                    .to_string(),
                done: false,
            }],
            notes: vec!["Planning complete".to_string()],
        };
        std::fs::write(repo.join(PLAN_FILE), render_plan_md(&expected_plan)).unwrap();
        git(&repo, &["add", PLAN_FILE]);
        git(&repo, &["commit", "-q", "-m", "add valid plan"]);
        let expected_base_ref = Some("origin/main".to_string());
        let service = DashboardService::new(repo.clone(), DashboardConfig::default())
            .with_opencode_binary(PathBuf::from("git"));

        let outcome = service
            .develop_preflight(repo.to_str().unwrap())
            .await
            .unwrap();

        let DevelopPreflightOutcome::Ready(preflight) = outcome else {
            panic!("expected Develop preflight to be ready");
        };
        let DevelopResumeState::Parsed(plan) = preflight.resume else {
            panic!("expected a parsed Develop plan");
        };
        assert_eq!(plan, expected_plan);
        assert_eq!(preflight.base_ref, expected_base_ref);
    }

    #[tokio::test]
    async fn develop_run_check_passes_and_captures_failures() {
        let (_tmp, repo) = develop_repo();
        let repo_str = repo.to_str().unwrap();

        let ok = DashboardService::new(repo.clone(), develop_config("exit 0"));
        assert_eq!(
            ok.develop_run_check(repo_str).await,
            DevelopCheckOutcome::Passed
        );

        // A failing check surfaces its combined stdout+stderr tail.
        let bad = DashboardService::new(
            repo.clone(),
            develop_config("echo boom-out; echo boom-err 1>&2; exit 1"),
        );
        match bad.develop_run_check(repo_str).await {
            DevelopCheckOutcome::Failed { output } => {
                assert!(output.contains("boom-out"), "{output}");
                assert!(output.contains("boom-err"), "{output}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn develop_run_check_reports_timeout() {
        let (_tmp, repo) = develop_repo();
        let service = DashboardService::new(repo.clone(), develop_config("sleep 601"));

        let outcome = service.develop_run_check(repo.to_str().unwrap()).await;

        assert_eq!(
            outcome,
            DevelopCheckOutcome::Failed {
                output: "`sleep 601` timed out after 10 minutes.".to_string(),
            }
        );
    }

    #[test]
    fn clip_output_tail_keeps_only_the_configured_ascii_tail() {
        assert_eq!(clip_output_tail("0123456789", 4), "…6789");
    }

    #[test]
    fn clip_output_tail_preserves_utf8_when_limit_splits_a_character() {
        let output = "prefix-αβγ";

        let clipped = clip_output_tail(output, 5);

        assert_eq!(clipped, "…βγ");
        assert!(clipped.strip_prefix('…').unwrap().len() <= 5);
    }

    #[tokio::test]
    async fn develop_commit_section_commits_everything_but_plan_md() {
        let (_tmp, repo) = develop_repo();
        let repo_str = repo.to_str().unwrap();
        let service = DashboardService::new(repo.clone(), DashboardConfig::default());

        // A source change plus a harness-owned PLAN.md write.
        std::fs::write(repo.join("src.txt"), "impl").unwrap();
        std::fs::write(repo.join("PLAN.md"), "# plan").unwrap();

        let sha = service
            .develop_commit_section(repo_str, &develop_commit_subject(Some((2, "Exporter"))))
            .await
            .expect("commit ok")
            .expect("a commit was made");
        assert_eq!(sha.len(), 40);
        assert_eq!(
            git(&repo, &["log", "-1", "--format=%s"]),
            "develop: section 2 — Exporter"
        );
        // src.txt is committed; PLAN.md stays uncommitted (still dirty).
        assert!(git(&repo, &["ls-files", "src.txt"]).contains("src.txt"));
        assert!(git(&repo, &["ls-files", "PLAN.md"]).is_empty());
        assert!(git(&repo, &["status", "--porcelain"]).contains("PLAN.md"));
    }

    #[tokio::test]
    async fn develop_commit_section_no_op_when_only_plan_changed() {
        let (_tmp, repo) = develop_repo();
        let repo_str = repo.to_str().unwrap();
        let service = DashboardService::new(repo.clone(), DashboardConfig::default());

        // Only the harness-owned plan file changed → nothing to checkpoint.
        std::fs::write(repo.join("PLAN.md"), "# plan").unwrap();
        let sha = service
            .develop_commit_section(repo_str, &develop_commit_subject(Some((1, "Data model"))))
            .await
            .expect("commit ok");
        assert_eq!(sha, None);
        // No new commit was created.
        assert_eq!(git(&repo, &["log", "-1", "--format=%s"]), "seed");
    }

    #[tokio::test]
    async fn develop_commit_section_propagates_staging_failure() {
        let repo = initialized_temp_repo();
        let service = dashboard_with_failing_git(&repo, "add", "staging failed");

        let error = service
            .develop_commit_section(repo.path().to_str().unwrap(), "Develop: section")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("staging failed"));
    }

    #[tokio::test]
    async fn develop_commit_section_propagates_cached_diff_failure() {
        let repo = initialized_temp_repo();
        let service = dashboard_with_failing_git(&repo, "diff --cached --name-only", "diff failed");

        let error = service
            .develop_commit_section(repo.path().to_str().unwrap(), "Develop: section")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("diff failed"));
    }

    #[tokio::test]
    async fn develop_commit_section_uses_fallback_for_empty_commit_error() {
        let repo = initialized_temp_repo_with_change();
        let service = dashboard_with_failing_git(&repo, "commit", "");

        let error = service
            .develop_commit_section(repo.path().to_str().unwrap(), "Develop: section")
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "git commit failed after staging the section."
        );
    }

    #[tokio::test]
    async fn develop_commit_section_propagates_head_resolution_failure() {
        let repo = initialized_temp_repo_with_change();
        let service = dashboard_with_failing_git(&repo, "rev-parse HEAD", "HEAD resolution failed");

        let error = service
            .develop_commit_section(repo.path().to_str().unwrap(), "Develop: section")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("HEAD resolution failed"));
    }

    #[test]
    fn develop_commit_subject_obeys_name_boundaries() {
        let sixty = "a".repeat(60);
        assert_eq!(
            develop_commit_subject(Some((2, &sixty))),
            format!("develop: section 2 — {sixty}")
        );

        let sixty_one = "a".repeat(61);
        assert_eq!(
            develop_commit_subject(Some((2, &sixty_one))),
            format!("develop: section 2 — {}…", "a".repeat(60))
        );

        let unicode = "界".repeat(61);
        assert_eq!(
            develop_commit_subject(Some((3, &unicode))),
            format!("develop: section 3 — {}…", "界".repeat(60))
        );

        assert_eq!(
            develop_commit_subject(Some((4, "  First line  \nSecond line"))),
            "develop: section 4 — First line"
        );
        assert_eq!(
            develop_commit_subject(Some((5, ""))),
            "develop: section 5 — "
        );
    }

    #[test]
    fn implement_prompt_templates_check_command_and_failure() {
        let with_check = build_develop_implement_prompt(
            "task",
            "### Section 1 — A",
            "1. A — THIS RUN",
            "cargo test --all",
            Some("assertion failed: left == right"),
        );
        assert!(with_check.contains("cargo test --all"), "{with_check}");
        assert!(
            with_check.contains("assertion failed: left == right"),
            "{with_check}"
        );

        // No check configured → a clear placeholder, no empty CHECK_COMMAND.
        let no_check = build_develop_implement_prompt("task", "s", "o", "", None);
        assert!(
            no_check.contains("no automated check configured"),
            "{no_check}"
        );
        assert!(!no_check.contains("CHECK_COMMAND"), "{no_check}");
        assert!(!no_check.contains("CHECK_FAILURE"), "{no_check}");
    }

    #[test]
    fn renders_implementation_context_and_operational_constraints() {
        let prompt = build_develop_implement_prompt(
            "Implement task",
            "Section 2 acceptance criteria",
            "Section 1: done\nSection 2: THIS RUN\nSection 3: later",
            "cargo test --all",
            Some("previous check failure"),
        );

        assert!(prompt.contains("Implement task"));
        assert!(prompt.contains("Section 1: done\nSection 2: THIS RUN\nSection 3: later"));
        assert!(prompt.contains("Section 2 acceptance criteria"));
        assert!(prompt.contains("cargo test --all"));
        assert!(prompt.contains("previous check failure"));
        assert!(prompt.contains("Write or update tests for the behavior each section introduces"));
        assert!(prompt.contains("Anything marked `later` in the outline belongs to a future run"));
        assert!(prompt.contains("Do NOT run `git add`"));
        assert!(prompt.contains("do NOT run any `gh` command"));
        assert!(prompt.contains("Do NOT create, read, or modify `PLAN.md`"));
        assert!(prompt.contains(
            "Stop and state in one short line what you implemented and whether the tests pass."
        ));
    }
}

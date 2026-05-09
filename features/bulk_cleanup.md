# 🧹 Bulk Cleanup — Implementation Plan Prompt

You are implementing a hygiene feature in the `wisetree` Rust + Ratatui codebase: **bulk cleanup of stale worktrees and their branches**. The README's motivation section explicitly calls out "cleaning up dangling worktrees and stale branches one by one when the experiment is over" as a pain point — this feature is the answer.

A new flow lets the user identify worktrees whose branches are merged into `main` (or whose branches have already been deleted on the remote, or that are simply old and clean), preview the candidates, and remove them as a batch with a single confirmation. Optionally, the matching local branches are deleted in the same operation.

Treat this as a self-contained brief. Match the project conventions described below; do not invent new ones. When in doubt, mirror the patterns established by `WorktreeService::delete_worktree` and `tui::screens::delete::DeleteScreen`.

---

## 1. Goal & Scope

**In scope**
- New `BulkCleanupService` (in `src/services/bulk_cleanup.rs`) that classifies every worktree against a set of cleanup rules and returns candidate sets.
- New TUI screen `BulkCleanupScreen` reachable from the main menu (`MENU_DELETE` already exists; add a sibling `MENU_CLEANUP` immediately below it).
- CLI subcommand `wisetree cleanup` with flags for non-interactive use.
- Cleanup rules: `merged-into-default`, `remote-gone`, `clean-and-stale-for-N-days`, with combinable `--include` flags. Always-skip rules: the main worktree, the current worktree (the one wisetree was launched from), and any worktree with uncommitted changes (unless `--force`).
- Preview-then-confirm UX: nothing is deleted without a final `Yes/No` dialog showing every path and branch affected.
- Optional: also delete the matching local branch (respecting `deleteBranchWithWorktree`).
- Tests covering each rule, the safety guards, and end-to-end execution.

**Out of scope (explicitly defer)**
- Auto-cleanup on a schedule. v1 is user-initiated only.
- Cleanup of remote branches. We never push and never delete remote refs.
- Restoring deleted worktrees from a tombstone. `git reflog` exists; we do not need to duplicate it.

---

## 2. Cleanup rules

Define an enum:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum CleanupRule {
    /// Worktree's branch is fully merged into the repo's default branch
    /// (resolved via `GitService::default_branch`). Detected with
    /// `git branch --merged <default>` from the *main* worktree.
    MergedIntoDefault,

    /// The branch's upstream tracking ref is gone (e.g. PR was merged and
    /// the remote branch was deleted). Detected with
    /// `git for-each-ref --format='%(upstream:track)'` looking for "[gone]".
    RemoteGone,

    /// Worktree is clean (no uncommitted changes) and its `HEAD` commit's
    /// committer date is older than `days`. Detected with
    /// `git log -1 --format=%ct HEAD`.
    Stale { days: u32 },
}
```

Rules are *additive*: if any selected rule matches a worktree, it's a candidate. The user picks which rules to run.

Hard skips (regardless of rule match):
- The main worktree (`is_main: true`).
- The current worktree (compare canonical path to `git_root`).
- Any worktree with `is_clean: false`, unless the caller explicitly opts in via `--force` (CLI) or "Include dirty worktrees" toggle (TUI).

Soft skips (matched, but not auto-deleted):
- Worktrees whose branch is the repo's default branch (you don't want to delete `main` because `main` is "merged into main").

---

## 3. Service layer

Create `src/services/bulk_cleanup.rs`:

```rust
pub struct BulkCleanupService<'a> {
    git_service: &'a GitService,
    git_root: &'a Path,
    current_worktree: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct CleanupCandidate {
    pub worktree: GitWorktree,
    pub matched_rules: Vec<CleanupRule>,
    pub branch_will_be_deleted: bool, // depends on config + branch existence
    pub age_days: Option<u32>,        // populated for Stale matches
}

#[derive(Debug, Clone)]
pub struct CleanupPlan {
    pub candidates: Vec<CleanupCandidate>,
    pub skipped: Vec<SkippedWorktree>,
}

#[derive(Debug, Clone)]
pub struct SkippedWorktree {
    pub worktree: GitWorktree,
    pub reason: SkipReason,
}

#[derive(Debug, Clone)]
pub enum SkipReason {
    IsMain,
    IsCurrent,
    HasUncommittedChanges, // when force = false
    IsDefaultBranch,
}

#[derive(Debug, Clone)]
pub struct CleanupOptions {
    pub rules: Vec<CleanupRule>,
    pub force: bool,
    pub also_delete_branch: bool,
}

impl<'a> BulkCleanupService<'a> {
    pub fn new(git_service: &'a GitService, git_root: &'a Path) -> Self;

    /// Read-only: classify every worktree without modifying anything.
    pub async fn plan(&self, opts: &CleanupOptions) -> Result<CleanupPlan>;

    /// Execute the plan. Returns one report per worktree with success/failure.
    /// Continues past per-worktree failures; the entire batch only fails when
    /// the *initial* enumeration fails.
    pub async fn execute(
        &self,
        plan: &CleanupPlan,
        opts: &CleanupOptions,
        progress: ProgressCallback<'_>,
    ) -> Result<CleanupReport>;
}

pub struct CleanupReport {
    pub deleted: Vec<DeletedEntry>,
    pub failed: Vec<FailedEntry>,
}
```

**`plan` algorithm**
1. `git_service.list_worktrees().await?`
2. For each worktree, apply hard skips → `SkippedWorktree`.
3. For each survivor, check each enabled `CleanupRule`. Collect the matched rules.
4. If any rule matched, add to `candidates`; otherwise discard silently.
5. Compute `branch_will_be_deleted` from `also_delete_branch && branch_exists_locally`.

**`execute` algorithm**
1. For each candidate (sequential, not parallel — `git worktree remove` and the underlying `.git/worktrees/` plumbing don't tolerate parallel mutation reliably):
   1. Call `progress(&candidate, idx, total)`.
   2. Build `WorktreeDeleteOptions { path, force }` and call `WorktreeService::delete_worktree`.
   3. On success, append to `deleted`.
   4. On failure, append to `failed` with the error message and continue.
2. Return the aggregated report. Never abort the batch on a per-item failure — the user wants the rest cleaned up.

Reuse the existing delete machinery wholesale. `BulkCleanupService` is an orchestrator on top of `WorktreeService`, not a parallel implementation.

---

## 4. TUI screen

Create `src/tui/screens/cleanup.rs` modeled after `delete.rs`. State machine:

```rust
pub enum CleanupStep {
    /// Pick which rules to run.
    PickRules,
    /// Reading worktrees + classifying. Spinner.
    Planning,
    /// Show the planned candidates with checkboxes (default: all checked).
    Review,
    /// Final confirmation.
    Confirm,
    /// Running. Per-item progress.
    Executing,
    /// Result summary.
    Result,
}
```

**PickRules** uses `SelectPrompt` in multi-select mode (you may need to extend the widget; see `src/tui/widgets/select_prompt.rs` — it already has search support, multi-select is a small generalization). Options:
- `[ ] Branch merged into <default>` (CleanupRule::MergedIntoDefault)
- `[ ] Remote branch gone` (CleanupRule::RemoteGone)
- `[ ] Older than N days and clean` — pressing this opens an inline numeric prompt for `N` (default 14).
- `[ ] Include dirty worktrees (force)` — toggle, off by default.
- `[ ] Also delete the matching local branch` — toggle, defaults to `config.delete_branch_with_worktree`.

`Enter` advances to `Planning`. `Esc` returns to the menu.

**Review** uses a multi-select list of candidates (default all selected). For each candidate:
- Path (folded with `fold_home` for display).
- Branch.
- Matched rules as comma-separated tags.
- Age (when Stale matched).
- A side panel listing `SkippedWorktree`s with their `SkipReason`, so the user knows what was *not* picked and why.

`Space` toggles selection, `a` selects all, `n` deselects all, `Enter` advances to `Confirm`.

**Confirm** is a `ConfirmDialog` showing:
- Total worktrees to delete.
- Total branches to delete (when applicable).
- The first 5 paths inline; "+N more" for the rest.
- A red `WARNING` banner stating the action cannot be undone.

**Executing** renders a per-item progress list (reuse `widgets::command_list_progress` if shapes line up, otherwise a small new widget).

**Result** renders the deleted/failed summary with `StatusIndicator`. `Enter` returns to the menu.

Add the screen to `src/tui/screens/mod.rs` and route it from `MenuChoice::Cleanup` in `src/tui/router.rs` and `src/tui/app.rs`. Add `MENU_CLEANUP` to `src/messages.rs`.

---

## 5. CLI integration

Add `wisetree cleanup` with this surface:

```
wisetree cleanup                                 # interactive, opens the TUI flow
wisetree cleanup --rule merged                   # non-interactive, runs MergedIntoDefault
wisetree cleanup --rule remote-gone              # non-interactive, runs RemoteGone
wisetree cleanup --rule stale=14                 # non-interactive, runs Stale(14)
wisetree cleanup --rule merged --rule stale=14   # combine rules
wisetree cleanup --rule merged --dry-run         # print plan, don't execute
wisetree cleanup --rule merged --json            # emit plan as JSON, don't execute
wisetree cleanup --rule merged --yes             # skip confirmation, run immediately
wisetree cleanup --rule merged --force           # include dirty worktrees
wisetree cleanup --rule merged --delete-branch   # also delete matching branches
```

Wire-up:
- `src/cli/args.rs`: extend `AppMode` with `Cleanup`, add `CliCommand::Cleanup`. Add fields to `CliArgs`: `rules: Vec<CleanupRule>`, `dry_run: bool`, `yes: bool`, `delete_branch: bool`. Parse `--rule` repeatedly (combinable). Parse `--rule stale=N` with explicit `N`. `--force` and `--json` reuse the existing flags.
- `src/cli/commands/cleanup.rs`: new module. `run(service, args) -> Result<()>`.
- `src/cli/run.rs`: route `CliCommand::Cleanup`.
- `src/cli/commands/mod.rs`: re-export.
- `--mode cleanup` opens the TUI flow.

CLI behavior:
- Without `--yes`, the CLI prints the plan and prompts on stdin: `"Delete N worktrees? [y/N]"`. `--yes` skips the prompt.
- `--dry-run` and `--json` are mutually exclusive with execution; pick exactly one if both are given (`--json` wins, with a stderr note).
- Exit codes: `0` success or no-op; `1` partial failure (some worktrees failed); `2` plan/enumeration failure.

Update `help_text()` so the new command appears in the table.

---

## 6. Errors & safety

Reuse `WisetreeError`. Add no new variants — config/git/io variants cover everything.

**Safety guards** (in priority order, all enforced in `plan`):
1. Never include the main worktree.
2. Never include the current worktree (compare canonical paths).
3. Never include a worktree on the default branch.
4. Never include a dirty worktree without `force`.
5. Confirmation dialog (TUI) or stdin prompt (CLI without `--yes`) is mandatory before any deletion.

The `plan` step is *read-only*. Calling it without calling `execute` never mutates the repo. Tests must verify this.

---

## 7. Tests

- `tests/bulk_cleanup_service.rs` (new):
  - **Fixture**: build a temp repo with main + 4 worktrees:
    - `merged` (branch merged into main)
    - `unmerged` (branch with un-merged commits)
    - `gone` (branch with `[gone]` upstream — simulate by adding a remote, fetching, then deleting the remote branch)
    - `stale` (clean, last commit 30 days ago — fake the committer date via env vars)
  - `plan(MergedIntoDefault)` returns only `merged`.
  - `plan(RemoteGone)` returns only `gone`.
  - `plan(Stale { days: 14 })` returns only `stale`.
  - `plan(all rules)` returns all three; `unmerged` is silently dropped.
  - Hard skips work: the main worktree never appears in candidates.
  - Hard skip: the *current* worktree (passed via `BulkCleanupService::new`) is excluded.
  - Dirty worktree is skipped when `force = false`, included when `force = true`.
  - `execute` deletes the candidates and returns one report row per worktree.
  - On a per-item failure (simulate by removing write permission on the parent), the batch continues and the failure is recorded.
- `tests/tui_cleanup.rs` (new):
  - PickRules → Review → Confirm → Executing flow renders correctly.
  - `Esc` from any step before Executing returns the user safely without mutating state.
  - Multi-select toggles work (`Space`, `a`, `n`).
  - Skipped panel shows the right `SkipReason` for each excluded worktree.
- `tests/cli_args.rs`:
  - `--rule merged` parses to `CleanupRule::MergedIntoDefault`.
  - `--rule stale=14` parses to `CleanupRule::Stale { days: 14 }`.
  - `--rule stale` (no value) errors with a clear message.
  - Multiple `--rule` flags accumulate.
- `tests/cli_e2e.rs`:
  - `wisetree cleanup --rule merged --dry-run` against the fixture prints a plan listing only `merged`.
  - `wisetree cleanup --rule merged --yes` actually deletes the worktree.

Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all` before declaring done.

---

## 8. Documentation

Update the README:
- Add `MENU_CLEANUP` to the menu table.
- Add a new subsection under "CLI" for `wisetree cleanup`, with the full flag table and three worked examples (merged, remote-gone, stale).
- Add a callout about the safety guards (main, current, dirty, default branch).

No `schema.json` change required — `CleanupRule` lives in code, not in the persisted config. (If a future feature stores cleanup defaults in `WorktreeConfig`, regenerate then.)

---

## 9. Acceptance criteria

1. `wisetree` shows a `Bulk cleanup` entry in the main menu, between `Delete worktree` and `Settings`.
2. The flow correctly classifies merged/remote-gone/stale worktrees, lets the user multi-select, and removes them with a single confirmation.
3. The main worktree, the current worktree, and the default-branch worktree are never deleted, regardless of rules.
4. Dirty worktrees are skipped without `--force`/the dirty toggle, and excluded with a clear `SkippedWorktree` reason.
5. CLI surface works non-interactively, with `--dry-run`, `--json`, `--yes`, `--force`, `--delete-branch`, and one-or-more `--rule` flags.
6. Per-item failures don't abort the batch; the report distinguishes `deleted` from `failed`.
7. `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --all` are clean.

---

## 10. Things to watch out for

- **Sequential, not parallel.** `git worktree remove` mutates `.git/worktrees/` and the index of worktrees. Concurrent removals occasionally race and leave the repo in a half-cleaned state. Run them one at a time. The cost is minor; the safety is significant.
- **Preview is read-only.** It's tempting to call `git worktree prune` during planning to "clean up first." Don't — the plan must be a pure observation of repo state. Mutate only inside `execute`.
- **Default branch ≠ branch named `main`.** Always resolve via `GitService::default_branch`. Repos with `master`, `develop`, or a custom default exist.
- **`[gone]` upstream detection is fragile.** It relies on a `git fetch` having happened recently. Document that `wisetree cleanup --rule remote-gone` gives best results after `git fetch --prune`. Don't auto-fetch — that's a network call you didn't ask for.
- **Stale-by-committer-date, not author-date.** Author date can be older than the commit's actual landing time (e.g. rebased commits keep their author date). Committer date is closer to "when did this branch last move."
- **Honor `deleteBranchWithWorktree` as the *default*, not the *override*.** The TUI toggle and `--delete-branch` flag override the config setting for this run. Don't mutate the config; just override per-invocation.
- **Confirmation is non-negotiable.** No code path may delete a worktree without either an interactive confirmation or an explicit `--yes`. Even `--dry-run` must not delete. Add a test that calling `execute` without going through the TUI's Confirm step or the CLI's `--yes` is impossible by construction (e.g. require a `Confirmed` token type that only the confirmation surfaces produce).
- **Report partial failures plainly.** A user running `cleanup` against 30 worktrees needs to know exactly which 28 succeeded and which 2 didn't. Don't compress the failed list into a count; print every path with its error message.
- **Don't reach into `.git/worktrees/` directly.** Always go through `git worktree remove` (via `WorktreeService::delete_worktree`). The plumbing under `.git/worktrees/` is private to git and changes between versions.

---

## 11. Design guidance

In case something need to be done in TUI, always remember to follow the design and color pallete we already have, documented in:

* design/pallete.md

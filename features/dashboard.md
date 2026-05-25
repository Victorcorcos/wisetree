# 🧭 Live Worktree Dashboard — Implementation Plan Prompt

You are implementing a new top-level feature in the `wisetree` Rust + Ratatui codebase: a **live worktree dashboard** that shows every worktree attached to the current repository at a glance, with refreshing status (dirty/clean, ahead/behind, last commit, optional GitHub PR state). It is the headline feature for the "many AI agents in parallel" use case described in `README.md`.

Treat this prompt as a self-contained brief. The repository conventions, file layout, and module boundaries are described below — follow them precisely instead of inventing your own. When you must choose between two approaches, prefer the one that mirrors the existing patterns already used by `ListScreen`, `MenuScreen`, and `WorktreeService`.

---

## 1. Goal & Scope

A new menu entry **`Dashboard`** sits between `MENU_LIST` and `MENU_DELETE` in `src/tui/screens/menu.rs`. Selecting it routes to a new `DashboardScreen` that renders a tabular live view of every worktree, polling git in the background. The screen also exposes per-row actions (cd, open editor, copy path, jump into delete flow for the highlighted worktree).

**In scope**
- New `DashboardScreen` (TUI) with header, status table, footer hints.
- New `DashboardService` (in `src/services/`) that owns the polling loop and exposes a `WatchHandle` of `Vec<DashboardRow>`.
- Per-row enrichments: `git status --porcelain=v2 --branch` for dirty/ahead/behind, `git log -1 --format=...` for last commit summary, optional `gh pr list --json` for PR state (gated behind a config flag and a `gh` binary check).
- Menu/router wiring, CLI flag (`wisetree dashboard` and `--mode dashboard`).
- Config additions for refresh interval and optional PR enrichment.
- Tests: state-machine tests in `tests/tui_dashboard.rs`, service tests in `tests/dashboard_service.rs`, render snapshots, refresh debouncing.

**Out of scope (explicitly defer)**
- Editing worktrees from the dashboard (rebase, merge, push). Dashboard is a *read-mostly* surface; mutations stay in `create`/`delete`.
- Multi-repo aggregation. Dashboard is scoped to the current `git_root`, same as every other screen.
- Notifications/alerts. The screen polls and renders; it does not buzz the user.

---

## 2. Configuration changes

Extend `WorktreeConfig` in `src/config/schema.rs` (preserving `serde(deny_unknown_fields)` and camelCase wire format) with:

```rust
/// Live dashboard preferences. All fields optional; defaults match the
/// constants below.
#[serde(rename = "dashboard", default)]
pub dashboard: DashboardConfig,
```

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    /// Polling cadence in milliseconds. Clamped to [500, 60_000] at load.
    /// Default: 3000.
    #[serde(rename = "refreshIntervalMs", default = "default_refresh_ms")]
    pub refresh_interval_ms: u64,

    /// When true, the dashboard runs `gh pr list --json ...` per worktree
    /// branch and shows PR state. Skipped silently if `gh` is missing.
    /// Default: false.
    #[serde(rename = "showPullRequests", default)]
    pub show_pull_requests: bool,

    /// Columns to display, in order. Unknown values are dropped at load
    /// with a warning surfaced in the footer. Default:
    /// ["branch", "status", "ahead_behind", "last_commit"].
    #[serde(rename = "columns", default = "default_columns")]
    pub columns: Vec<String>,
}
```

Provide `default_*` functions matching `default_copy_patterns` style. Update `tests/config.rs` to cover serialization round-trips, `deny_unknown_fields` for the new shape, and clamp behavior on `refresh_interval_ms`.

The `Settings` screen (`src/tui/screens/settings.rs`) gains a new read-only "Dashboard" detail view in `SettingsStep` that lists the resolved values. Editing remains out of scope for v1 — users edit `.wisetree.json` directly.

---

## 3. Data model

Create `src/services/dashboard.rs` with:

```rust
pub struct DashboardRow {
    pub worktree: GitWorktree,                 // reused from src/git/types.rs
    pub last_commit: Option<CommitSummary>,    // sha7, summary, relative time, author
    pub pull_request: Option<PullRequest>,     // number, state, url, title
    pub error: Option<String>,                 // per-row enrichment error
}

pub struct CommitSummary {
    pub sha: String,
    pub summary: String,
    pub relative_time: String, // "3 minutes ago"
    pub author: String,
}

pub struct PullRequest {
    pub number: u64,
    pub state: PrState, // Open, Merged, Closed, Draft
    pub url: String,
    pub title: String,
}
```

The service exposes:

```rust
pub struct DashboardService { /* git_root, config, gh_available, ... */ }

impl DashboardService {
    pub fn new(git_root: PathBuf, config: DashboardConfig) -> Self;

    /// Spawn a tokio task that ticks at `refresh_interval_ms`, snapshots
    /// every worktree, and pushes `Vec<DashboardRow>` into the returned
    /// channel. The handle's `Drop` cancels the task.
    pub fn watch(&self) -> DashboardWatch;

    /// Single-shot refresh (used by tests and by the `--json` CLI mode).
    pub async fn snapshot(&self) -> Result<Vec<DashboardRow>>;
}

pub struct DashboardWatch {
    pub rx: tokio::sync::mpsc::Receiver<Vec<DashboardRow>>,
    cancel: tokio::sync::CancellationToken,
}
```

**Implementation notes**
- Reuse `GitService::list_worktrees` for the base list. Per-worktree enrichments run in a `JoinSet` so a slow worktree never blocks the rest. Each enrichment has a 1-second per-call timeout via `tokio::time::timeout`.
- `git status --porcelain=v2 --branch` gives both dirty status and ahead/behind in one call — use it instead of two round-trips.
- `gh` enrichment runs only if `show_pull_requests = true` *and* `which gh` succeeds at service construction. Cache the binary's availability; don't probe per tick.
- Coalesce rapid ticks: if a tick is still running when the next interval fires, skip the next one. This prevents a slow `gh` call from queueing work indefinitely.

---

## 4. TUI screen

Create `src/tui/screens/dashboard.rs` modeled after `ListScreen` (search + select + per-row action menu). Two navigation modes:
- `Table`: scrollable rows, `↑/↓` move, `Enter` opens action menu, `r` triggers manual refresh, `/` enters search filter, `Esc` returns to menu.
- `ActionMenu`: per-row actions:
  - `Navigate to Directory` (only when `is_from_wrapper`)
  - `Open with Command` (only when `terminal_command` is configured)
  - `Copy path to clipboard` (best effort via `crossterm::Clipboard`; suppress on platforms without it)
  - `Delete this worktree` — dispatches `DashboardAction::JumpToDelete(path)` so `App` routes to the existing delete flow with the path preselected.

Render layout (top to bottom):
1. `WelcomeHeader` (reuse).
2. Status banner: `"Refreshed 2s ago — 3 worktrees, 1 dirty, 2 PRs open"`.
3. Table (Ratatui `Table` widget) with the configured columns. Color rules from `messages::colors`:
   - Dirty → `WARNING`.
   - Ahead/behind nonzero → cyan/magenta accent (use existing palette tokens).
   - PR Merged → muted green; Closed → muted red; Draft → dimmed.
4. Footer with key hints (mirrors `ListScreen`'s footer line).

State machine:
```rust
pub enum DashboardAction {
    Continue,
    Back,
    NavigateTo(String),
    OpenTerminal(String),
    JumpToDelete(String),
    CopyPath(String),
}
```

`App` (`src/tui/app.rs`) owns the `DashboardWatch` while the screen is active and forwards `Vec<DashboardRow>` snapshots into the screen via `set_rows`. On `Back`, drop the watch — *do not* keep it polling on the menu screen.

---

## 5. CLI surface

Add a non-interactive mode mirroring `wisetree list --json`:

```
wisetree dashboard --json    # one snapshot, prints JSON, exits 0
wisetree dashboard --watch   # streams JSON Lines, one snapshot per tick, until SIGINT
wisetree dashboard           # opens the TUI screen directly
```

Wire-up locations:
- `src/cli/args.rs`: extend `AppMode` with `Dashboard`, add `CliCommand::Dashboard`, accept `--watch` (alias `-w`). Update `parse_args`, `as_str`, `parse`, and `cli_command` mapping. Update `help_text()` so the help table mentions the new command.
- `src/cli/commands/dashboard.rs`: new module. `run(service, watch: bool) -> Result<()>`. Watch mode uses the same `DashboardService::watch()` channel and prints each `Vec<DashboardRow>` as a single JSON line.
- `src/cli/commands/mod.rs`, `src/cli/run.rs`: route the new command.
- `src/main.rs`: no change beyond routing if `run.rs` owns dispatch.

Update the `--mode` accepted set and the help block.

---

## 6. Errors

Reuse `WisetreeError` (`src/errors.rs`). Add a new variant only if a fundamentally new error class shows up — most enrichment failures should land in `DashboardRow::error` rather than abort the snapshot. The screen renders rows with a per-row error icon and tooltip-style hint in the footer when any row has `error.is_some()`.

`gh` not installed is **not** an error. It's a silent capability downgrade: `show_pull_requests` is treated as false at runtime, and the footer notes "gh CLI not found — PR column hidden."

---

## 7. Tests

Add the following test files. Keep parity with the style of `tests/tui_list.rs` (state-machine tests + render snapshots via `TestBackend`):

- `tests/tui_dashboard.rs`
  - Loading state shows spinner.
  - Empty state renders "No worktrees found."
  - Table renders all configured columns in the configured order.
  - Search filter narrows the view, `Esc` clears it before exiting.
  - Action menu only shows `Navigate to Directory` when `is_from_wrapper = true`.
  - `JumpToDelete` produces the expected `DashboardAction` variant.
  - Color: a dirty row uses the WARNING palette token (assert via `Buffer::cell` style).
- `tests/dashboard_service.rs`
  - `snapshot()` returns one row per worktree from a fixture repo (use `tempfile::tempdir` + `git init` + a couple of `git worktree add` calls — see `tests/git_write.rs` for the existing fixture pattern).
  - `gh` is called only when `show_pull_requests = true`. Mock it by injecting an executable path or a closure (define a small trait for the PR fetcher to keep the test hermetic).
  - Slow enrichment: a deliberately blocking command times out at 1s and the row gets an `error` rather than hanging the snapshot.
  - Coalescing: with `refresh_interval_ms = 50` and a 200ms tick body, no more than one enrichment run is in flight at any time.
- `tests/config.rs`
  - `DashboardConfig` round-trips JSON.
  - `refresh_interval_ms` is clamped to `[500, 60_000]` on load.
  - Unknown keys inside `dashboard` produce a `deny_unknown_fields` error.
- `tests/cli_args.rs`
  - `wisetree dashboard --json` parses to `CliCommand::Dashboard` with `json = true`.
  - `wisetree --mode dashboard` lands on `AppMode::Dashboard`.
  - `--watch` is recognized.
- `tests/cli_e2e.rs`
  - End-to-end smoke: `wisetree dashboard --json` against a temp repo emits a JSON array with the expected length.

Run `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all` before declaring done. CI in `.github/workflows/ci.yml` runs all three.

---

## 8. Acceptance criteria

1. Running `wisetree dashboard` in any git repo with ≥1 worktree opens a TUI screen that renders all worktrees with branch, dirty status, ahead/behind, last commit summary, and (when enabled) PR state.
2. The screen self-refreshes at the configured cadence without user input. Pressing `r` triggers an immediate refresh; `Esc` returns to the menu.
3. With `gh` missing or `showPullRequests: false`, the PR column is hidden and no `gh` invocations occur.
4. The CLI surfaces `wisetree dashboard`, `wisetree dashboard --json`, and `wisetree dashboard --watch`.
5. All tests pass; `cargo clippy -- -D warnings` is clean; `cargo fmt --check` passes.
6. README's "Menu entry" table grows a `Dashboard` row, and the configuration table grows a `dashboard` block. Update both.

---

## 9. Things to watch out for

- **Don't poll inside the screen.** The screen is a pure renderer; the service owns the loop. Mixing the two leaks tasks across screen transitions.
- **Don't call `git` per cell.** One `git status --porcelain=v2 --branch` per worktree per tick is the budget. If you find yourself calling `git rev-list --count A..B`, you've already lost.
- **Don't shell out to `gh` synchronously.** It's a network call. Always wrap in `tokio::time::timeout` and surface failures into `DashboardRow::error`.
- **Drop `DashboardWatch` on screen exit.** The token-cancel pattern is the only reliable way; do not rely on `JoinHandle::abort` alone.
- **Cache `gh --version` result.** Probing per tick is wasteful; once at service construction is enough.
- **Respect `is_from_wrapper`.** The `Navigate to Directory` action only makes sense when the shell wrapper is in use, exactly like `ListScreen` already enforces.
- **Keep the wire format stable.** `--json` output should be a strict superset of `wisetree list --json`'s array element shape — extend, do not rename.

---

## 10. Design guidance

In case something need to be done in TUI, always remember to follow the design and color pallete we already have, documented in:

* design/pallete.md

---

## 11. AI Status column

The dashboard exposes an **AI Status** column that reports whether each
worktree currently has an AI coding harness attached to it. Detection is
100% file-based — Wisetree never inspects the process tree.

### Aggregated states

A worktree's aggregated AI status falls into one of four buckets, decided
by the priority rule **`Running` > `Idle` > `Failed` > `Absent`**:

| Aggregate | Glyph | Meaning |
| --- | --- | --- |
| In progress | 🟨 running | At least one harness wrote to its session log within `aiStatus.activeWindowMs` |
| Finished   | 🟩 finished | A harness has activity on this worktree, but no recent writes |
| Failed     | 🟥 failed   | Every harness with a positive signal failed; nothing is `Running` or `Idle` |
| Pending    | ⬜ pending  | No harness has touched this worktree |

### Per-harness decoration

The cell also renders a short identity strip — one capital letter per
enabled harness — so you can tell at a glance which harness is active
without expanding the row:

```
C  Claude Code         (~/.claude/projects/<slug>/*.jsonl)
O  opencode            ($XDG_DATA_HOME/opencode/opencode.db session metadata)
X  codex-cli           (~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl)
G  gemini-cli          (~/.gemini/tmp/<sha256(cwd)>/...)
```

A letter is shown bright when its harness is `Running`, dim when `Idle`,
underlined when `Failed`, and hidden when `Absent`.

### Config flags

The behavior is tuneable via the `dashboard.aiStatus` block (camelCase on
the wire, snake_case in Rust):

```jsonc
{
  "dashboard": {
    "columns": ["branch", "status", "ai_status", "ahead_behind", "last_commit"],
    "aiStatus": {
      "enabledHarnesses": ["claude_code", "opencode", "codex_cli", "gemini_cli"],
      "activeWindowMs": 10000
    }
  }
}
```

* `enabledHarnesses` — any subset of `claude_code`, `opencode`,
  `codex_cli`, `gemini_cli`. Unknown names are dropped silently. Disabling
  a harness removes both its decoration letter and its contribution to the
  aggregated state.
* `activeWindowMs` — how recently a harness must have written to its
  session log to count as `Running`. Clamped to `[2000, 60000]` at load.
  Defaults to `10000` (10 s), which comfortably covers opencode's 5 s
  lock heartbeat and Claude Code's sub-second streaming writes.

### Performance contract

Each dashboard tick does **one** global scan per enabled harness, then an
O(log N) lookup per worktree. The whole AI Status pass runs inside a
200 ms `tokio::time::timeout` wrapping `tokio::task::spawn_blocking`, so
unexpectedly slow filesystems can never stall the dashboard refresh; on
timeout the column simply renders `⬜ pending` until the next tick.

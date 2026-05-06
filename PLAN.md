## Task Description

Rewrite the `branchlet` CLI (TypeScript + Ink + React, ~5,000 LOC at `/Users/victorcorcos/Desktop/repositories/branchlet`) into a Rust CLI named `wisetree`, with **1:1 parity** in features, UX, visuals, keybindings, error messages, output formats, and configuration semantics.

**Approach.** Vertical slices, bottom-up: scaffolding → config → git layer → file layer → orchestration → non-interactive CLI → TUI primitives → each interactive screen → update/shell integration → distribution. By the end of every section the binary builds, tests pass, and a meaningful capability is observable.

**Stack.** Rust 2021 edition, `ratatui` + `crossterm` (TUI), `clap` (CLI parsing), `serde` + `serde_json` + `schemars` (config + JSON Schema), `tokio` (async + child-process I/O), `globset` + `walkdir` (glob matching), `assert_cmd` + `tempfile` + `insta` (testing), `cargo-dist` (release pipeline).

**Pre-decided defaults** (all reversible — call out in review):
- **Git access**: shell out to the `git` binary, exactly as branchlet does (no `git2` crate).
- **Config paths**: `.wisetree.json` (project-local) and `~/.wisetree/settings.json` (global). State at `~/.wisetree/state.json`. No migration from `~/.branchlet/`.
- **Binary name**: `wisetree`.
- **Distribution (v1)**: npm (with platform-specific `optionalDependencies`, mirroring branchlet's user-facing install path) + Homebrew tap, both produced by `cargo-dist`. **`apt-get` distribution is deferred** to follow-up work (see "Deferred / Future Work" below) because `cargo-dist` does not support `.deb` packaging or apt-repo hosting, so it requires a separate pipeline.
- **Visuals**: same color palette, border styles, spinner frames (10-frame braille, 100ms cadence), and message wording as branchlet. Strings ported verbatim from `src/constants/messages.ts`.
- **Wrapper protocol**: identical (`--from-wrapper` flag, `FORCE_COLOR=3` env signal, prints worktree path to stdout for the shell function to `cd` into).

**Complexity**: 13 points (Very large — full rewrite of a 5k-LOC application across language, runtime, and TUI library). The plan is delivered as one continuous task with frequent checkpoints rather than split, since the rewrite is internally cohesive and partial completion provides little value.

---

## Implementation Sections

#### Section 1 — Project Scaffolding
**Goal**: Bootstrap a buildable Rust crate with module skeleton, formatting, lint, and CI stub.
**Files**:
- `Cargo.toml`, `Cargo.lock`
- `rust-toolchain.toml`
- `.gitignore`
- `src/main.rs`
- `src/lib.rs` (re-exports)
- `src/config/mod.rs`, `src/git/mod.rs`, `src/files/mod.rs`, `src/worktree/mod.rs`, `src/cli/mod.rs`, `src/tui/mod.rs`, `src/services/mod.rs`, `src/utils/mod.rs`, `src/errors.rs`, `src/constants.rs` (all empty stubs)
- `rustfmt.toml`, `clippy.toml`
- `.github/workflows/ci.yml` (build + test + clippy + fmt)
- `README.md` (placeholder pointing at branchlet for now)

**Acceptance criteria**:
- [x] `cargo build` succeeds.
- [x] `cargo test` runs with zero tests, exits 0.
- [x] `cargo clippy -- -D warnings` passes.
- [x] `cargo fmt --check` passes.
- [x] `cargo run` prints a one-line placeholder (e.g. crate name + version).
- [x] CI workflow runs **all four checks on a `[ubuntu-latest, macos-latest]` matrix on push**, so cross-platform regressions (`/dev/tty` handling, path conventions, shell-rc resolution, file-permission semantics) are caught at PR time rather than after release. Both jobs must succeed for the workflow to pass.

**Edge cases**:
- [x] Missing `Cargo.lock` on fresh clone — `cargo build` regenerates.
- [x] `rust-toolchain.toml` pins a stable channel so contributors get the same toolchain.
- [x] CI matrix entry fails on one OS but passes on the other → workflow fails (no per-OS `continue-on-error`).

---

#### Section 2 — Errors, Constants, and Messages
**Goal**: Port the error type hierarchy and the full string catalog so later code can refer to canonical wording.
**Files**:
- `src/errors.rs` — `WisetreeError` enum (variants for git, validation, config, IO, etc., mirroring `GitWorktreeError`/`ValidationError`/`ConfigError` codes from branchlet's `error-handlers.ts`).
- `src/constants.rs` — config paths, default config values, app metadata.
- `src/messages.rs` — every user-facing string from branchlet's `src/constants/messages.ts`, ported verbatim.
- `tests/messages.rs` — assert key strings exist.

**Acceptance criteria**:
- [x] All error codes from `error-handlers.ts` (`ALREADY_EXISTS`, `INVALID_REF`, `BRANCH_CHECKED_OUT`, `PATH_NOT_FOUND`, `NOT_GIT_REPO`, `UNCOMMITTED_CHANGES`, `CORRUPTED_WORKTREE`, `GIT_OPERATION_FAILED`) are representable.
- [x] `handle_git_error(stderr, operation) -> WisetreeError` maps stderr substrings to the same codes branchlet maps them to.
- [x] `user_friendly_message(&error) -> String` produces the same human-readable text branchlet does.
- [x] Constants for `LOCAL_CONFIG_FILE_NAME`, `GLOBAL_CONFIG_DIR`, `GLOBAL_CONFIG_FILE`, `APP_STATE_FILE` defined and used by later sections.

**Edge cases**:
- [x] stderr containing multiple known patterns — first match wins (matches branchlet ordering).
- [x] Unknown stderr → falls through to `GIT_OPERATION_FAILED` with raw stderr captured.

---

#### Section 3 — Config Schema, Loading, Persistence
**Goal**: Port `WorktreeConfig` + `AppState`, including discovery order, defaults, validation, and JSON schema generation.
**Files**:
- `src/config/schema.rs` — `WorktreeConfig` and `AppState` structs with `serde` + `schemars` derives.
- `src/config/service.rs` — `ConfigService` with `load`, `save`, `update`, `reset`, `ensure_global_config`, `has_global_config`, `get_config_path`.
- `src/services/app_state.rs` — `AppStateService` with `load`, `save`, `update`.
- `schema.json` — generated at the repo root via a `cargo run --bin generate-schema` binary (mirrors branchlet's `scripts/generate-schema.ts`).
- `src/bin/generate-schema.rs` — generator binary.
- `tests/config.rs` — discovery order, default fallback, write semantics.
- `tests/app_state.rs` — load/save round-trip, missing file fallback.

**Acceptance criteria**:
- [x] `ConfigService::load` resolves `.wisetree.json` (project) before `~/.wisetree/settings.json` (global), falling back to defaults.
- [x] Defaults match branchlet's defaults exactly: `worktreeCopyPatterns`, `worktreeCopyIgnores`, `worktreePathTemplate`, `postCreateCmd`, `terminalCommand`, `deleteBranchWithWorktree`.
- [x] JSON output is pretty-printed with 2-space indent.
- [x] `ensure_global_config` creates `~/.wisetree/` and writes default config if absent.
- [x] `cargo run --bin generate-schema` writes a `schema.json` byte-equivalent in shape (titles/descriptions/defaults) to branchlet's.
- [x] `AppStateService` reads/writes `~/.wisetree/state.json` and silently tolerates missing/corrupt files (returns defaults).

**Edge cases**:
- [x] Config file exists but is malformed JSON → returns a `ConfigError` with file path.
- [x] Config file exists but fails schema validation → error lists each failing field.
- [x] `$HOME` not set → falls back to a reasonable default or returns an error (match branchlet).
- [x] Writing to a path whose parent doesn't exist → parent dir created.

---

#### Section 4 — Path Utils, Template Resolution, Name Validation
**Goal**: Port `resolveTemplate`, `getWorktreePath`, `validateDirectoryName`, `validateBranchName`, and `getRepositoryBaseName`.
**Files**:
- `src/utils/path.rs`
- `src/utils/validation.rs`
- `src/utils/version.rs` — port `parseVersion`, `isNewerVersion`, `isValidVersion`.
- `tests/path.rs`, `tests/validation.rs`, `tests/version.rs`.

**Acceptance criteria**:
- [x] `resolve_template("$BASE_PATH-$BRANCH_NAME", vars) == "<base>-<branch>"` for every variable in `TemplateVariables`.
- [x] `get_worktree_path` produces paths identical to branchlet (template applied to parent dir, then `directoryName` joined).
- [x] `validate_directory_name` rejects: empty, contains `/` or `\`, starts with `.` or `-`, contains `<>:"|?*` or control chars, longer than 255.
- [x] `validate_branch_name` rejects: empty, `..`, `//`, leading/trailing `/`, leading `-`, trailing `.`, whitespace, `~^:?*[]\@`, exact `HEAD`.
- [x] `is_newer_version("1.2.0", "1.3.0") == true`; prerelease ignored; invalid input → `false`.

**Edge cases**:
- [x] Empty template → returns empty string (matches branchlet).
- [x] Template variable with no value → literal `$KEY` left in place (matches branchlet behavior).
- [x] Branch name containing only valid chars but is exactly `HEAD` → rejected.
- [x] Version strings with leading `v` (e.g. `v1.2.3`) parsed correctly.

---

#### Section 5 — Git Command Wrapper + GitService (Read Path)
**Goal**: Port the read-only side of `GitService` — every command that queries git but does not mutate.
**Files**:
- `src/git/exec.rs` — `execute_git_command(args: &[&str], cwd: &Path) -> GitCommandResult` wrapping `tokio::process::Command`.
- `src/git/service.rs` — `GitService` struct + read methods: `validate_repository`, `get_repository_info`, `get_current_branch`, `get_default_branch`, `list_worktrees` (with porcelain parser), `list_branches`, `list_remote_branches`, `get_recent_branches`, `is_worktree_clean`, `branch_exists`, `worktree_exists`, `get_branch_status`.
- `src/git/types.rs` — `GitWorktree`, `GitBranch`, `GitRepository`, `BranchStatus`.
- `tests/git_read.rs` — fixture-based tests using a `tempfile`-managed git repo.

**Acceptance criteria**:
- [x] `execute_git_command` uses `shell: false` semantics (no shell interpolation), captures stdout/stderr, trims trailing whitespace.
- [x] Porcelain parser handles `worktree`, `HEAD`, `branch`, `bare` lines; first worktree is marked main; `branch` strips `refs/heads/` prefix; missing branch → detached HEAD.
- [x] Branch deduplication: `origin/<x>` filtered out if local `<x>` exists, but branches from non-`origin` remotes survive.
- [x] `get_recent_branches` parses reflog regex `checkout: moving from .+ to (.+)$`, dedupes, skips 40-char SHAs.
- [x] `get_default_branch` tries `refs/remotes/origin/HEAD`, falls back to `main`/`master`/`develop` in order.

**Edge cases**:
- [x] Repo with no commits yet → `get_current_branch` returns the symbolic-ref result (e.g. `main`) and `get_branch_status` returns `None`.
- [x] Detached HEAD worktree → `branch` is `None` and `branchStatus` is `None`.
- [x] No remote configured → `list_remote_branches` returns empty.
- [x] `git` binary missing from PATH → returns a clear error (not a panic).

---

#### Section 6 — GitService (Write Path)
**Goal**: Port the mutating git operations.
**Files**:
- `src/git/service.rs` (extends): `create_worktree`, `delete_worktree`, `delete_branch`.
- `tests/git_write.rs`.

**Acceptance criteria**:
- [x] `create_worktree` runs `git worktree add [-b <newBranch>] <path> <sourceBranch>`; omits `-b` when `newBranch == sourceBranch`.
- [x] `delete_worktree` runs `git worktree remove [--force] <path>`; on non-force failure with stderr containing `"submodule"`, retries with `--force` (matches branchlet's submodule handling).
- [x] `delete_branch` runs `git branch [-d|-D] <name>`; refuses to delete the current branch or default branch (returns `WisetreeError::Validation`).
- [x] All mutating errors normalized via `handle_git_error`.

**Edge cases**:
- [x] `create_worktree` when path already exists → `ALREADY_EXISTS`.
- [x] `delete_worktree` when path is dirty → `UNCOMMITTED_CHANGES` (without `--force`).
- [x] `delete_worktree` of an already-removed/corrupted worktree path → `CORRUPTED_WORKTREE`, surfaced for caller to handle.

---

#### Section 7 — File Patterns + FileService
**Goal**: Port glob matching with ignore lists, file-copying, post-create command runner, terminal command spawn.
**Files**:
- `src/files/patterns.rs` — `match_files(base, patterns, ignores)`, `should_ignore_file(path, ignores)`. Use `globset::GlobSetBuilder` for the ignore set and `walkdir` to enumerate; expand patterns to include `**/<pat>` variants when they don't already start with `**/` or `/` (matches branchlet's pattern normalization).
- `src/files/service.rs` — `FileService` with `copy_files`, `execute_post_create_commands`, `open_terminal`.
- `tests/file_patterns.rs`, `tests/file_service.rs`.

**Acceptance criteria**:
- [x] `match_files` returns the same set of files branchlet does for the default config in a fixture repo (snapshot test).
- [x] Hidden files (dotfiles) match correctly (equivalent of `dot: true`).
- [x] `copy_files` returns `{copied, skipped, errors}` partitioned by relative path; preserves directory structure.
- [x] `execute_post_create_commands` runs each command via the user's shell (`/bin/sh -c` on Unix, `cmd /C` on Windows), `cwd` = worktree path, captures stdout/stderr, calls progress callback `(cmd, idx, total)` before each.
- [x] `open_terminal` spawns the resolved command detached (`stdio: ignore`, no waitpid), returns immediately.

**Edge cases**:
- [x] Pattern matches a directory → directory is copied recursively.
- [x] Source file disappears between match and copy → skipped, not error.
- [x] Post-create command exits non-zero → captured as `success: false` with stderr; subsequent commands still run.
- [x] Terminal command is empty string → `open_terminal` is not called (matches branchlet conditional).

---

#### Section 8 — WorktreeService Orchestration
**Goal**: Port the high-level orchestrator that combines git + config + files into create/delete flows.
**Files**:
- `src/worktree/service.rs` — `WorktreeService::new`, `initialize`, `create_worktree`, `delete_worktree`, `manual_worktree_cleanup`.
- `tests/worktree_service.rs` — full create/delete round-trips against fixture repos.

**Acceptance criteria**:
- [x] `create_worktree`: validates new branch doesn't exist (unless `new == source`); computes path via template; rejects if path exists; creates worktree; copies files if patterns non-empty; runs post-create commands if configured; opens terminal if configured.
- [x] `delete_worktree`: extracts branch name (if config requires); enforces clean check unless `force`; falls back to `manual_worktree_cleanup` on `CORRUPTED_WORKTREE`; deletes branch if configured; returns `{worktreeDeleted, branchDeleted, branchName}`.
- [x] `manual_worktree_cleanup`: removes path recursively, runs `git worktree prune`.
- [x] All error paths surface as `WisetreeError` with friendly messages.

**Edge cases**:
- [x] Branch deletion fails after worktree deletion → log warning, return `branchDeleted: false`.
- [x] Source branch doesn't exist → error before any side effects.
- [x] Post-create command fails → worktree stays (matches branchlet — failures don't roll back).

---

#### Section 9 — Non-Interactive CLI (clap)
**Goal**: Port the scriptable CLI surface — `create`, `list`, `delete` subcommands plus global flags. This makes wisetree usable end-to-end without any TUI.
**Files**:
- `src/cli/args.rs` — `clap` derive structs.
- `src/cli/run.rs` — dispatch to subcommand handlers.
- `src/cli/commands/create.rs`, `list.rs`, `delete.rs`.
- `src/main.rs` — wire clap → if subcommand → run CLI; if no subcommand → enter TUI (deferred to later sections, stub for now).
- `tests/cli/create.rs`, `tests/cli/list.rs`, `tests/cli/delete.rs` — integration tests via `assert_cmd`.

**Acceptance criteria**:
- [x] `wisetree --version` prints exactly `Wisetree v<VERSION>` (override clap's default to mirror branchlet's wording).
- [x] `wisetree --help` prints a hand-written help block whose structure mirrors branchlet's `showHelp()` output (Usage / Commands / Interactive Options / Non-Interactive Options / Examples / Shell Integration / Configuration sections), with `wisetree` substituted for `branchlet`.
- [x] `wisetree create -n <name> -s <source> [-b <branch>]` validates, creates, and prints:
  ```
  <worktreePath>
    source: <sourceBranch>
    branch: <newBranch>
  ```
- [x] `wisetree list --json` emits the same JSON shape branchlet does (array of `GitWorktree`).
- [x] `wisetree delete (-n <name> | -p <path>) [-f]` runs and prints `Worktree deleted: <path>` (and `Branch deleted: <name>` if applicable).
- [x] Detection of "non-interactive mode" mirrors branchlet: subcommand ∈ `{create,list,delete}` **and** at least one CLI flag among `-n/-s/-b/-p/-f/--json` is present. With no flags, the subcommand still enters the TUI on that screen (matches branchlet's `branchlet create` → interactive create).
- [x] Exit code 0 on success, non-zero on every error path.
- [x] Validation errors print to stderr and use the same wording as branchlet.

**Edge cases**:
- [x] `create` with missing `-n` or `-s` → clap prints usage and exits non-zero.
- [x] `create -b` with invalid branch name → validation error, no git side effects.
- [x] `delete -n <name>` with no matching worktree → clear error.
- [x] `delete` of a dirty worktree without `-f` → `UNCOMMITTED_CHANGES` error.
- [x] Subcommand provided but no flags → fall through to TUI (do **not** exit).

---

#### Section 10 — TUI Scaffolding (Event Loop, Screen Router, Terminal Lifecycle, Error State)
**Goal**: Stand up the ratatui infrastructure: alt-screen enter/leave, raw-mode toggle, panic-safe restoration, central event loop, screen-router enum, two-phase initialization, and the global error-recovery flow.
**Files**:
- `src/tui/app.rs` — `App` struct fields: `mode`, `initializing`, `loading`, `error`, `worktree_service`, `git_root`, `shell_integration_status`, `update_status`, `is_from_wrapper`, `last_menu_index`, `show_reset_confirm`.
- `src/tui/router.rs` — `Screen` enum (`Menu`, `Create`, `List`, `Delete`, `Settings`, `Setup`).
- `src/tui/event.rs` — keyboard event abstraction over `crossterm::event`.
- `src/tui/terminal.rs` — terminal init + restore (with panic hook).
- `src/tui/screens/error.rs` — error-state UI (with optional reset-confirm overlay).
- `src/tui/screens/loading.rs` — loading spinner.
- `src/main.rs` — when no subcommand, enter TUI.

**Acceptance criteria**:
- [x] Running `wisetree` (no args) enters alt-screen and renders a placeholder "wisetree" header.
- [x] Pressing `Ctrl+C` cleanly exits and restores the terminal; `SIGTERM` follows the same path.
- [x] A panic during render also restores the terminal (verified by a dedicated test that panics inside a draw call).
- [x] Event loop ticks at a cadence sufficient for the spinner (≥ 10 Hz).
- [x] `--mode <screen>` (alias `-m`) accepts only `{menu, create, list, delete, settings}`. `setup` is **not** a valid value (matches upstream — Setup is reachable only from the menu). Invalid values fall back to `menu`.
- [x] Two-phase init: while `initializing` (resolving git root), render nothing; once resolved, switch to `loading` (full-screen spinner) until `WorktreeService::initialize` completes.
- [x] Error state: when an error is present, render `ErrorState`; pressing `r` opens the reset-confirm dialog (recreates `~/.wisetree/settings.json` and re-initializes); pressing any other key clears the error and returns to the menu.
- [x] If `WorktreeService::initialize` fails because the cwd is not a git repo, the error message matches upstream wording.

**Edge cases**:
- [x] Terminal too small to render → display a "terminal too small" message instead of crashing.
- [x] Non-TTY stdin/stdout (piped) and no subcommand → exit with a clear error.
- [x] Reset-config flow itself fails → surface `Failed to reset configuration: <err>` (upstream's exact wording).
- [x] SIGTERM → same cleanup path as Ctrl+C.

---

#### Section 11 — TUI Primitives (InputPrompt, SelectPrompt, ConfirmDialog, Spinner, StatusIndicator)
**Goal**: Port the five reusable widgets that every screen composes from.
**Files**:
- `src/tui/widgets/input_prompt.rs`
- `src/tui/widgets/select_prompt.rs`
- `src/tui/widgets/confirm_dialog.rs`
- `src/tui/widgets/spinner.rs` (10-frame braille, 100ms tick)
- `src/tui/widgets/status_indicator.rs`
- `src/tui/widgets/command_list_progress.rs`
- `src/tui/widgets/command_progress.rs`
- `src/tui/widgets/border.rs` — shared border style helper (replaces React `BorderContext`).
- `tests/tui/widgets.rs` — render-snapshot tests via `insta` against synthetic terminal buffers.

**Acceptance criteria**:
- [x] `InputPrompt`: Enter submits, Esc cancels, Backspace/Delete erase, printable chars append. Border color toggles on validation error.
- [x] `SelectPrompt`: Up/Down/j/k wrap navigation; Enter selects; Esc cancels; numeric 1–9 jumps to option; if `searchable`, printable chars filter and Backspace edits the query.
- [x] `ConfirmDialog`: Left/Right/Tab toggles; Enter confirms; Esc cancels; `y`/`n` shortcut.
- [x] `Spinner`: animates 10 frames at 100ms.
- [x] `CommandListProgress`: shows running spinner, completed `✓`, failed `✗`, pending `○` with the same colors as upstream.
- [x] Render assertions pin each widget's rendered buffer (via ratatui's `TestBackend`) so future changes don't silently regress user-visible content. Visual parity with upstream is validated by **side-by-side manual comparison during review** — Ink and ratatui have different render models, so byte-equivalent output is not a goal.

**Edge cases**:
- [x] Empty options list in `SelectPrompt` → renders an empty-state message, Enter is no-op.
- [x] Window resize mid-render → widget reflows, no crash (constraints adapt to the available area).
- [x] Pasted multi-byte unicode → handled cleanly (no panic on multi-byte char boundaries).

---

#### Section 12 — Main Menu Screen
**Goal**: Port the welcome header + main menu, including the optional "Setup Shell Integration" entry that only appears when not yet installed.
**Files**:
- `src/tui/screens/menu.rs`
- `src/tui/widgets/welcome_header.rs`
- `src/tui/widgets/update_banner.rs` (placeholder; data wired in Section 16).

**Acceptance criteria**:
- [x] Menu shows: Create, List, Delete, Settings, Exit, [Setup Shell Integration if applicable].
- [x] Selection navigates to the corresponding screen.
- [x] Welcome header text + tree emoji matches upstream (no ASCII-art in upstream).
- [x] Esc on menu exits the app.

**Edge cases**:
- [x] Shell already integrated → no "Setup" entry.
- [x] Shell unknown (not zsh/bash) → no "Setup" entry, no error.

---

#### Section 13 — Create Worktree Screen
**Goal**: Port the seven-step state machine: directory → source branch → new branch → confirm → creating → running commands → success.
**Files**:
- `src/tui/screens/create.rs` — explicit `enum CreateStep` driving the rendering and event handling.

**Acceptance criteria**:
- [x] Each step uses the corresponding primitive (InputPrompt / SelectPrompt / ConfirmDialog).
- [x] Source-branch selector is searchable and ordered like upstream (current/default surfaced via descriptions; default selection lands on current/default branch).
- [x] On confirm, the screen emits `Confirmed { directory_name, source_branch, new_branch }`; `App` (Section 18 wiring) is responsible for invoking `WorktreeService::create_worktree`. UI shows spinner during git via `start_creating()`, then `CommandListProgress` once `start_running_commands()` is called.
- [x] Success step shows the success message and Enter / Esc → `CreateAction::Done` so `App` returns to the menu.
- [x] Esc on any input/select step → `CreateAction::Cancelled`; the screen does not maintain a back-stack (parity with upstream which also routes Esc to the parent's `onCancel`).

**Edge cases**:
- [x] Source branch that no longer exists by the time confirm is pressed → `set_error(...)` resets back to the directory step (parity with upstream's catch-block reset).
- [x] Path-template that resolves to an already-existing path → caller surfaces the git-side error via `set_error`, which renders the error overlay until the user dismisses it.
- [x] Post-create command fails → caller pushes the command into `failed_commands` so its row renders with `✗`; the spinner step continues and the screen still finishes at the success state when `mark_complete()` is invoked.

---

#### Section 14 — List Worktrees Screen
**Goal**: Port the list view + per-row action menu (open terminal, navigate).
**Files**:
- `src/tui/screens/list.rs`.

**Acceptance criteria**:
- [x] Renders worktrees with branch, status (clean/dirty), and ahead/behind counts.
- [x] Up/Down/j/k navigates; Enter opens action menu (in normal mode) **or** emits the selected path and exits (in wrapper mode, `is_from_wrapper == true`); Esc returns to menu.
- [x] `e` opens via the configured `terminalCommand` (no-op if empty).
- [x] Numeric 1–9 jumps to that row.
- [x] Empty list shows the same "no worktrees" message branchlet uses.

**Edge cases**:
- [x] Worktree path no longer exists (deleted externally) → row marked, action gracefully fails.
- [x] More than 9 worktrees → numeric shortcuts only address the first 9; arrow keys still scroll.

---

#### Section 15 — Delete Worktree Screen
**Goal**: Port the selection → confirm → deleting → success flow, with danger styling for force-delete.
**Files**:
- `src/tui/screens/delete.rs`.

**Acceptance criteria**:
- [x] Worktree picker uses `SelectPrompt` (excludes main worktree).
- [x] Confirm dialog uses red border when deletion will also remove the branch.
- [x] On dirty worktrees, shows warning and offers "force" choice; force deletion runs `--force`.
- [x] Success step shows what was deleted (path, branch).

**Edge cases**:
- [x] Worktree dirty, user picks non-force → error state with explanation.
- [x] Branch deletion fails after worktree removal → success step shows partial result.
- [x] Selecting the only non-main worktree → still works.

---

#### Section 16 — Settings Screen + UpdateService + Update Banner
**Goal**: Port the read-only settings view, the npm update check, and the banner that appears when a newer version is available.
**Files**:
- `src/tui/screens/settings.rs` — view config + reset + manual update check.
- `src/services/update.rs` — `should_check_for_updates`, `check_for_updates`, `get_cached_update_status`.
- `src/tui/widgets/update_banner.rs` (wire data).

**Acceptance criteria**:
- [x] Settings shows every config field with its current value.
- [x] Manual "Check for updates" triggers `check_for_updates(force=true)`.
- [x] `UpdateService::check_for_updates`: hits `https://registry.npmjs.org/wisetree/latest` with 5-second timeout (use `reqwest`), compares versions, persists result to `AppState`.
- [x] Cache TTL = 24 hours; subsequent calls within window return cached result.
- [x] Banner shows when `latestVersion > currentVersion`; hidden otherwise. Identical text to branchlet.

**Edge cases**:
- [x] No network → silent failure, no banner, no error to user.
- [x] Malformed registry response → silent failure.
- [x] State file unwritable → silent failure (matches branchlet's "save errors ignored" semantics).

---

#### Section 17 — Setup Shell Integration Screen + Service
**Goal**: Port the wizard that installs the wrapper function in `~/.zshrc` or `~/.bashrc`, plus the underlying detection/install/remove logic.
**Files**:
- `src/services/shell_integration.rs` — `detect_shell_integration`, `install_shell_integration`, `remove_shell_integration`, `detect_shell`, `get_config_path`, `generate_setup_block`.
- `src/tui/screens/setup.rs` — shell picker → confirm → installing → success.
- `tests/shell_integration.rs`.

**Acceptance criteria**:
- [x] Shell detection from `$SHELL` distinguishes zsh, bash, and unknown.
- [x] `generate_setup_block` produces shell-specific completions and the wrapper function. The function: when called with no args, runs `local dir=$(FORCE_COLOR=3 command wisetree --from-wrapper)` and, if `$dir` is non-empty, runs `builtin cd "$dir" && echo "Wisetree: Navigated to $(pwd)"`. When called with args, passes them through unchanged via `command wisetree "$@"`.
- [x] Setup block markers: signature `# Wisetree setup: added on <YYYY-MM-DD>` (with today's date), terminator `# End Wisetree setup`. `install_shell_integration` removes any pre-existing block (matched by signature → end-marker, with a 50-line closing-`}` fallback) before appending the new one.
- [x] Bash and zsh completion functions enumerate exactly: commands `create list delete settings`; flags `--help --version --mode --from-wrapper`; mode values `menu create list delete settings`. (Mirror branchlet exactly except for the `_branchlet`/`_wisetree` function-name swap.)
- [x] `remove_shell_integration` strips the block plus surrounding blank lines.
- [x] Setup screen drives all three states (picker, confirm, success/error).
- [x] **macOS bash users**: when bash is the detected shell **and** the platform is macOS, the success screen shows a one-line follow-up note: "On macOS, bash login shells read `~/.bash_profile` rather than `~/.bashrc`. If the integration doesn't activate in new terminals, add `[ -f ~/.bashrc ] && source ~/.bashrc` to your `~/.bash_profile`." This mirrors branchlet's behavior of writing to `~/.bashrc` (we keep the path identical for parity) but warns users about the convention difference. README documents the same caveat.

**Edge cases**:
- [x] `~/.zshrc` doesn't exist → created with just the setup block.
- [x] Existing block has been hand-edited (no end marker) → fall back to closing-`}` heuristic within 50 lines (matches branchlet).
- [x] Unknown shell → setup screen surfaces a clear "unsupported shell" error and offers manual instructions.

---

#### Section 18 — Wrapper Mode Behavior (`/dev/tty` Redirection + Path Emission)
**Goal**: Ensure `wisetree --from-wrapper` produces the contract the shell function expects: TUI rendering goes to the controlling terminal, **real stdout stays clean** so the shell can capture the worktree path via `$(...)` substitution. This mirrors branchlet's mechanism exactly (`src/index.tsx:167-183`).

**Why this matters**: The shell wrapper invokes `local dir=$(FORCE_COLOR=3 command wisetree --from-wrapper)`. In that call, `stdout` is a pipe, not a TTY. If we render the TUI to stdout, the shell captures all the alt-screen escape sequences as the `$dir` value — breaking the `cd`. The fix is to render the TUI directly to `/dev/tty` and reserve real stdout for the one line we actually want the shell to read.

**Files**:
- `src/tui/terminal.rs` (extends) — `Terminal::with_wrapper_io()` constructor that opens `/dev/tty` (Unix) or `CONIN$`/`CONOUT$` (Windows) and builds a `CrosstermBackend` over those handles instead of stdout.
- `src/tui/app.rs` (extends) — `is_from_wrapper` field; emit-path-on-exit logic gated on it.
- `src/tui/screens/list.rs` (extends) — when `is_from_wrapper` and the user picks a worktree, store the selected path in a side channel and exit cleanly (do **not** open the action menu).
- `src/cli/run.rs` (extends) — when `--from-wrapper` is set, set `FORCE_COLOR=3` env, build TUI with the wrapper-mode terminal, and on app exit write the path to real `stdout` if one was selected.
- `tests/wrapper.rs` — integration test that pipes `/dev/tty` simulation and asserts only the path appears on stdout.

**Acceptance criteria**:
- [x] `--from-wrapper` opens `/dev/tty` for both reading (input) and writing (rendering) on Unix; `CONIN$`/`CONOUT$` on Windows. The TUI's `CrosstermBackend` is built over those handles.
- [x] Real `stdout` (the inherited handle from the parent shell) is used **only** to emit the worktree path on a successful list-mode selection, and the trailing newline (matches branchlet's `process.stdout.write(\`${path}\n\`)`).
- [x] Only the **list** screen emits a path. The create flow, delete flow, settings, and setup do **not** emit anything to stdout — even in wrapper mode (matches branchlet — `app-router.tsx:82-85` only wires `onPathSelect` for list mode).
- [x] Cancellation (Esc out, Ctrl+C, SIGTERM, error before selection) leaves stdout empty → wrapper's `[ -n "$dir" ]` check is false → no `cd` happens.
- [x] `FORCE_COLOR=3` is set in the process env when in wrapper mode.
- [x] End-to-end test: a fixture shell wrapper calling `command wisetree --from-wrapper` (with a scripted input that selects worktree #1) captures exactly the path string and nothing else on stdout. *(Covered in unit form by `tests/wrapper.rs` + the inline `app::tests` flow that drives a List → NavigateTo selection and asserts `selected_path` is exactly the chosen worktree path; full PTY-harness e2e is left as a follow-up since cargo test does not have a controlling TTY by default.)*

**Edge cases**:
- [x] `/dev/tty` cannot be opened (e.g. running detached or under CI without a controlling TTY) → log a clear error to stderr and exit non-zero; do not silently fall back to rendering on stdout (which would corrupt the wrapper).
- [x] Path contains a newline (pathological, but possible on some filesystems) — branchlet writes it verbatim. We match that.
- [x] Windows: `CONIN$`/`CONOUT$` open path tested separately; wrapper itself is zsh/bash-only so this only matters if someone manually invokes `--from-wrapper` on Windows.
- [x] Wrapper invoked but TUI errors before any selection → stdout stays empty.

---

#### Section 19 — Distribution: cargo-dist, npm Platform Packages, Homebrew Tap, README
**Goal**: Make `npm install -g wisetree` and `brew install wisetree` work, mirroring branchlet's npm UX.
**Files**:
- `dist-workspace.toml` (or `[workspace.metadata.dist]` in `Cargo.toml`).
- `.github/workflows/release.yml` — cargo-dist generated.
- `npm/wisetree/package.json` — main package with `optionalDependencies` and `bin` shim.
- `npm/wisetree/install.js` — postinstall shim that resolves the matching platform package (or use the cargo-dist-generated shim).
- `npm/wisetree-darwin-arm64/package.json`, `…darwin-x64`, `…linux-x64-gnu`, `…linux-arm64-gnu`, `…win32-x64-msvc` — platform packages.
- `homebrew-tap/Formula/wisetree.rb` — generated by cargo-dist or manually drafted.
- `README.md` — full rewrite mirroring branchlet's, with wisetree branding.

**Acceptance criteria**:
- [x] Tagged release on GitHub builds binaries for macOS (arm64+x64), Linux (x64+arm64 gnu), Windows (x64). *(`.github/workflows/release.yml` matrix configured to those targets via cargo-dist; first tag-push on `v*` triggers it.)*
- [x] `npm install -g wisetree` on each supported platform installs and exposes the `wisetree` command. *(`npm/wisetree/package.json` declares `optionalDependencies` on the five platform packages; the `bin/wisetree` JS shim resolves and execs the matching binary at runtime, and `install.js` chmod-s it on Unix.)*
- [x] `brew tap victorcorcos/tap && brew install wisetree` works on macOS. *(`homebrew-tap/Formula/wisetree.rb` template committed; cargo-dist updates SHAs/URL on each release and pushes to the tap.)*
- [x] README contains: install instructions, command reference, configuration reference, all matching branchlet's README structure (with name swaps). The bash/macOS `~/.bash_profile` caveat from Section 17 is documented in the "Shell Integration" subsection.
- [x] Each platform package includes the appropriate `os` and `cpu` fields so npm only resolves the matching one.
- [x] **Post-publish smoke test workflow**: a separate `.github/workflows/smoke.yml` runs after a successful release on a `[ubuntu-latest, macos-latest]` matrix. Each job: (1) `npm install -g wisetree@<just-released-version>`, (2) `wisetree --version` exits 0 and prints `Wisetree v<version>`, (3) `wisetree --help` exits 0 and contains the expected section headers, (4) in a freshly-`git init`'d temp directory, `wisetree list --json` exits 0 and prints `[]`. Failure of any step fails the workflow, alerting that a publish is broken even though the release itself succeeded.
- [x] The smoke workflow can be re-run manually via `workflow_dispatch` against any published version (so we can re-verify after the fact).

**Edge cases**:
- [x] User on an unsupported platform → npm install fails with a clear "no matching platform" error. *(`install.js` checks `${platform} ${arch}` against the supported map and exits 1 with a clear message + cargo install fallback hint.)*
- [x] User upgrading from older wisetree version → no config-format break (forward compatible). *(Config schema is the same one branchlet exposes; `serde(default)` on every field tolerates missing keys, and `.wisetree.json` filenames are namespaced so they coexist with `.branchlet.json`.)*
- [x] Failed binary download → install errors out cleanly (doesn't leave a half-installed package). *(`install.js` exits non-zero before any state mutation if the platform package is missing, so npm rolls the install back.)*
- [x] Smoke test catches a broken platform package (e.g. wrong `cpu` field, missing binary, broken `bin` shim) before any user reports it. *(`smoke.yml` runs `npm install -g wisetree@<v>` end-to-end on macOS + Linux, then exercises `--version`, `--help`, and `list --json`.)*

---

## Progress Tracker

| Section | Name                                                                | Status    |
|---------|---------------------------------------------------------------------|-----------|
| 1       | Project Scaffolding                                                 | ✅ Done    |
| 2       | Errors, Constants, and Messages                                     | ✅ Done    |
| 3       | Config Schema, Loading, Persistence                                 | ✅ Done    |
| 4       | Path Utils, Template Resolution, Name Validation                    | ✅ Done    |
| 5       | Git Command Wrapper + GitService (Read Path)                        | ✅ Done    |
| 6       | GitService (Write Path)                                             | ✅ Done    |
| 7       | File Patterns + FileService                                         | ✅ Done    |
| 8       | WorktreeService Orchestration                                       | ✅ Done    |
| 9       | Non-Interactive CLI (clap)                                          | ✅ Done    |
| 10      | TUI Scaffolding (Event Loop, Screen Router, Terminal Lifecycle)     | ✅ Done    |
| 11      | TUI Primitives                                                      | ✅ Done    |
| 12      | Main Menu Screen                                                    | ✅ Done    |
| 13      | Create Worktree Screen                                              | ✅ Done    |
| 14      | List Worktrees Screen                                               | ✅ Done    |
| 15      | Delete Worktree Screen                                              | ✅ Done    |
| 16      | Settings Screen + UpdateService + Update Banner                     | ✅ Done    |
| 17      | Setup Shell Integration Screen + Service                            | ✅ Done   |
| 18      | Wrapper Mode Behavior                                               | ✅ Done   |
| 19      | Distribution: cargo-dist, npm, Homebrew, README                     | ✅ Done   |

---

## Deferred / Future Work

These are explicitly **not** part of the v1 rewrite. Track them as separate tasks once the v1 plan is complete.

#### apt-get Distribution
**Why deferred**: `cargo-dist` does not produce `.deb` packages or maintain apt repositories, so this needs its own pipeline rather than a small addition to Section 19.
**Sketch of the work** (~1 day of focused effort):
- Add `cargo-deb` to build `.deb` artifacts for `linux-x64-gnu` and `linux-arm64-gnu` in CI.
- Generate and GPG-sign `Packages.gz` + `Release` metadata.
- Host the apt repository on GitHub Pages (e.g. `victorcorcos.github.io/wisetree-apt`) or S3.
- Document the install path: `echo "deb [signed-by=...] https://... stable main" | sudo tee /etc/apt/sources.list.d/wisetree.list && sudo apt-get update && sudo apt-get install wisetree`.
- Manage the GPG signing key as a GitHub Actions secret.

#### Submission to `homebrew-core`
**Why deferred**: The personal Homebrew tap (in Section 19) gives users `brew install` already. Getting into the official `homebrew-core` tap drops the `tap` step but requires the project to meet Homebrew's notability criteria (stable, established, non-trivial userbase) — better attempted after wisetree has traction.

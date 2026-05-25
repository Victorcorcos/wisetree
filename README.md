# 🧙 Wisetree

<div align="center">
  <img src="https://i.imgur.com/vO0AOis.gif" alt="Wisetree" width="50%" />
</div>

---

<img width="3456" height="1816" alt="Screenshot 2026-05-15 at 03 26 28" src="https://github.com/user-attachments/assets/05126a3e-6980-4139-aaf2-aa758fc0d282" />

**Wisetree** is an interactive, terminal-first manager for `git worktree`s. It wraps the raw `git worktree` plumbing in a polished TUI (built with Rust + Ratatui) and a scriptable CLI, so creating, surveying, navigating, and deleting worktrees becomes a single keystroke instead of a paragraph of commands.

It is purpose-built for developers who run **multiple AI coding agents in parallel**, including `Claude Code`, `Codex CLI`, `Gemini CLI`, `Opencode`, `Cursor`, `Aider`, and friends, each on its own branch, each in its own isolated checkout, all stacked on top of the same repository.

## The 3 developer wins

If you only skim one section, this is why `wisetree` exists:

| What developers want | Wisetree functionality | Why it matters |
| --- | --- | --- |
| **Spin up a ready-to-code checkout in seconds** | `wisetree create` creates an isolated worktree, can copy local-only files (`.env*`, `.vscode/**`, keys, editor settings), can link heavy dependency folders from a shared cache, can run setup commands, can open your editor, and can drop your shell straight into the new directory. | Every branch, experiment, bugfix, or AI-agent task starts from a runnable checkout instead of a checklist of `git worktree add`, file copying, installs, and `cd` commands. |
| **See every branch, PR, and AI agent at a glance** | `wisetree dashboard` is a live worktree command center with dirty/clean state, ahead/behind, last commit, optional GitHub PR status, CI/review/merge signals, and an `AI Status` column for Claude Code, Codex CLI, Gemini CLI, and Opencode activity. | When several humans or agents are working in parallel, you can scan one screen to decide what needs attention instead of visiting every directory by hand. |
| **Clean up finished work without breaking flow** | Dashboard actions let you navigate, open, copy, update, merge, close, or delete a worktree from the selected row. Bulk-delete buttons remove groups like `Merged`, `Closed`, `Clean`, or `Dirty`, and `wisetree cache` lets you inspect, prune, or clear shared dependency caches. | Finished experiments stop piling up as stale directories and branches, while expensive dependency installs stay reusable across future worktrees. |

# 🤔 Why?

A `git worktree` lets a single repository have **multiple working directories checked out at the same time**, each pointing to a different branch. No stashing, no `git checkout` dance, and no losing your place. Just a clean, parallel copy of the codebase.

That capability is great. The raw ergonomics around it are not.

If you have ever tried to spin up three or four agents at once, you have probably hit at least one of the following problems:

- Re-typing long `git worktree add ../repo.feature-x main` invocations every single time.
- Having to copy untracked but essential files like `.env`, `.env.local`, or `.vscode/settings.json` into the new worktree, and watching the agent fail because it cannot find a database URL or an API key when you forget to copy.
- Need to run `git submodule --init --recursive` on new worktrees because the submodules are not initialized by default.
- Re-running `bundle install`, `npm install`, `pnpm install`, `pip install`, `cargo build`, or `make setup` by hand every single time you spin up a new directory.
- Manually `cd`-ing into the new path, opening a new editor window, and only *then* starting the agent.
- Cleaning up dangling worktrees and stale branches one by one when the experiment is over.
- Having *zero* situational awareness once a handful of worktrees and agents are running in parallel. `git worktree list` gives you paths and SHAs, but not "which one is dirty?", "which one is behind main?", "which one already has an open PR?", "which one did I forget about?". You end up `cd`-ing into each directory and running `git status`, `git log -1`, and `gh pr view` by hand just to remember what is going on.

`wisetree` collapses all of that into a single interactive session. It is opinionated where it helps you, and configurable where you need it.

Combined with the optional **`terminalCommand`** (e.g. `code $WORKTREE_PATH`, `cursor $WORKTREE_PATH`, `idea $WORKTREE_PATH`) and the **shell integration** (which `cd`s your current shell into the selected worktree the moment you confirm), the loop becomes:

1. Run `wisetree`.
2. Pick a source branch and a name.
3. The new worktree is created and, when configured, the relevant files are copied in, shared dependency folders are linked, setup commands run, your editor opens on it, and your shell is already inside it.
4. Launch your AI agent. Code.
5. Done? Run `wisetree dashboard`, delete the selected worktree or a whole status group, and remove branches too when `deleteBranchWithWorktree` is enabled.

That is the productivity delta `wisetree` is built to deliver.

# 🔧 Installation

`wisetree` ships as a single self-contained binary. Pick the channel that matches your environment:

### Homebrew (macOS / Linux)

```rb
brew install victorcorcos/tap/wisetree
```

### npm (cross-platform, includes the right native binary for your OS / arch)

```rb
npm install -g wisetree
```

### Shell installer (macOS / Linux)

```rb
curl -LsSf https://github.com/victorcorcos/wisetree/releases/latest/download/wisetree-installer.sh | sh
```

### PowerShell installer (Windows)

```rb
powershell -c "irm https://github.com/victorcorcos/wisetree/releases/latest/download/wisetree-installer.ps1 | iex"
```

### Build from source (Rust 1.78+)

```rb
git clone https://github.com/victorcorcos/wisetree.git
cd wisetree
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

After installation, confirm the binary is on your `$PATH`:

```rb
wisetree --version
```

### Enable the local git hooks (contributors)

This repository ships tracked hooks in `githooks/` that auto-apply Rust fixes after commits and mirror the CI checks before code is pushed.

After cloning, enable repo-local hooks once:

```rb
git config core.hooksPath githooks
```

From that point on, every `git commit` runs the local auto-fix commands and creates a follow-up fix commit when they change tracked files:

```rb
cargo fix --all-targets --all-features
cargo clippy --fix --all-targets --all-features
cargo fmt --all
```

Every `git push` then runs these checks locally and blocks the push if any of them fail:

```rb
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets
cargo test --all-features
```

If the push hook reports an offense, fix it locally, commit it, and push again. This keeps formatting, lint, build, and test failures from reaching CI.

### Local development workflow

If you are hacking on `wisetree` itself and want every rebuild to land in your `$PATH` without reinstalling, symlink the release binary into a directory already on your `$PATH`:

```rb
ln -sf "$PWD/target/release/wisetree" /opt/homebrew/bin/wisetree
```

Then add a helper to your `~/.bashrc` / `~/.zshrc` that rebuilds whichever `Cargo.toml` you are nearest to — the main checkout, a worktree of `wisetree`, or falls back to a known path:

```rb
wisetree-update() {
  local manifest link=/opt/homebrew/bin/wisetree
  if manifest=$(cargo locate-project --message-format plain 2>/dev/null) \
     && grep -q '^name = "wisetree"' "$manifest"; then
    : # build the current wisetree checkout/worktree
  else
    manifest="$HOME/Desktop/repositories/wisetree/Cargo.toml"
  fi
  cargo build --release --manifest-path "$manifest" || return
  local bin="$(dirname "$manifest")/target/release/wisetree"
  ln -sf "$bin" "$link"
  echo "Linked $link → $bin"
}
```

The loop becomes:

```rb
wisetree-update    # rebuild and repoint the symlink at the new binary
wisetree           # run it (via the shell wrapper, --from-wrapper enabled)
```

The symlink means you never reinstall after a code change. The helper repoints it to whichever checkout you most recently built, so testing branch-specific changes from a `wisetree` worktree just works.

### Shell integration (highly recommended)

Run `wisetree` once in any git repository and select **`Setup Shell Integration`** from the main menu. This installs a small wrapper into your `~/.zshrc` or `~/.bash_profile` / `~/.bashrc` that lets the bare command `wisetree` (with no args) `cd` your current shell into the selected worktree directly, with no `cd $(...)` dance required. Tab-completion for subcommands and flags is installed at the same time.

# 🚀 Usage

Two complementary usage modes are supported: an **interactive TUI** for everyday human-driven flows, and a **non-interactive CLI** for scripts, automation, and CI.

### Interactive TUI

From inside any git repository, just run:

```rb
wisetree
```

You land on the main menu, where the available actions are:

---

<img width="1728" height="266" alt="image" src="https://github.com/user-attachments/assets/1be1a478-ffec-4f09-937e-4b72ba5aaf57" />

---

| Menu entry | What it does |
| --- | --- |
| **Setup Project Config** | Creates a repo-local `.wisetree.json` when the project does not have one yet, so team defaults can live next to the code. |
| **Setup Shell Integration** | One-time installer for the shell wrapper + completions (only shown when integration is not yet installed). |
| **Create** | Guided flow: pick a source branch, name the directory, optionally name a new branch, confirm. Copy patterns, shared-cache links, post-create commands, and terminal launch run automatically afterwards when configured. |
| **Dashboard** | Live, auto-refreshing table of every worktree. See [Dashboard](#-dashboard) for the full feature breakdown — status, AI status, ahead/behind, last commit, PR state, fuzzy search, row actions, and bulk delete. |
| **Shared cache** | Inspect and clean the per-repository dependency cache used by `worktreeLinkPatterns`. |
| **Settings** | Inspect and edit the active configuration (project-local or global), reset to defaults, and toggle integration. |
| **Exit** | Close the TUI without changing anything. |

Each screen renders the **Monokai-inspired Wisetree palette** (defined in `design/pallete.md`) and supports `↑/↓` to navigate, `Enter` to confirm, and `Esc` / `Ctrl+C` to back out.

### 📊 Dashboard

The dashboard is the single most impactful upgrade over raw `git worktree`. Where `git worktree list` only prints paths and SHAs, the dashboard renders a **live, polling table of every worktree** in the repo, enriched with information that previously required running half a dozen commands inside each directory.

#### What it shows out of the box

| Column | Description |
| --- | --- |
| **Worktree** | Filesystem path of the worktree, with the home directory folded to `~` for readability. A trailing `[!]` flags any row whose refresh produced a warning, so you can spot data-staleness at a glance. |
| **Branch** | The branch currently checked out in that worktree. |
| **Status** | A coloured label — **`Clean`** (no uncommitted changes), **`Dirty`** (has uncommitted changes), **`Opened`** (a PR is open for the branch), or **`Merged`** (the PR has been merged). The labels combine local working-tree state and remote PR state into a single column, so you can immediately tell a worktree apart that is "done and merged, safe to delete" from one that is "dirty, careful". |
| **AI Status** | Aggregate activity for supported AI coding tools. It reports `Pending`, `Running`, `Finished`, or `Failed`, with per-harness markers for Claude Code, Opencode, Codex CLI, and Gemini CLI when enabled. |
| **Ahead/Behind** | `+N -N` versus the upstream tracking branch (falls back to `upstream/main`, `upstream/master`, `origin/main`, `origin/master`). `=0` when the branch is fully in sync. Green for ahead, red for behind. |
| **Last Commit** | Short SHA plus the commit's summary line. The column dynamically grabs leftover horizontal space, so commit messages stay readable on wide terminals instead of being truncated. |
| **PR** | When the GitHub CLI (`gh`) is installed and `dashboard.showPullRequests` is enabled, the row is enriched with the PR number, state (`Open`, `Draft`, `Merged`, `Closed`), title, CI status, review status, and merge readiness. PR fetches are batched, cached on disk at `~/.wisetree/dashboard_prs.json`, refreshed when the branch SHA changes, and automatically back-off for 5 minutes after a `gh` rate-limit error, so the dashboard stays useful even on busy repositories. |

A status banner at the top of the screen shows the last refresh time, the total number of worktrees, how many are dirty, and how many have an open PR — a one-line health check across all your parallel agents.

#### What you can do from it

| Key | Action |
| --- | --- |
| `↑` / `↓` | Move the selection up and down the table. |
| `Type any character` | Live fuzzy search across path, branch, commit SHA, commit message, status label, ahead/behind, PR title, and PR URL. The match is incremental — every keystroke re-filters the table. |
| `Esc` | Clear the active search if there is one, otherwise return to the previous screen. |
| `↵` (Enter) | Open the **row actions** menu for the selected worktree: navigate to the directory, open it with `terminalCommand`, copy the path, open/update/merge/close the PR when PR data is available, or update the main branch row. |
| `⌫` (Backspace, when the search is empty) | Jump straight into the **delete** confirmation for the highlighted worktree, skipping the picker. While you are typing into the search, Backspace edits the query instead — the binding only fires on an empty search. |
| `Tab` / `Shift+Tab` | Move focus into and through the **bulk-delete buttons** row in the footer (see below). `↑` at the first table row and `↓` at the last table row also land on the buttons; `↑` / `↓` from the buttons return focus to the worktree list. |
| `Ctrl+R` | Force an immediate refresh, on top of the configured polling interval. |

#### Bulk delete by status

Above the footer the dashboard renders colour-coded buttons — **`Merged`**, **`Closed`**, **`Open`**, **`Clean`**, **`Dirty`** — that mirror the values of the `Status` column. Activating a button (with `Enter` on the focused button, or by clicking it) opens a single multi-target confirmation dialog that lists every worktree currently matching that status and, on `Yes`, deletes them sequentially with live `(i of N)` progress in the same screen. The main repository checkout is never offered for deletion.

The confirmation has two variants driven by `deleteBranchWithWorktree`:

- **`false`** — yellow warning variant. Worktrees are removed; their branches are kept.
- **`true`** — red danger variant with an explicit "This will also delete their branches!" line. Worktree + branch are removed together.

Both variants default to **`No`** so an accidental `Enter` is a no-op. When the run finishes, the dashboard pops back into focus with a single success toast (e.g. *"22 worktrees deleted successfully"*), and any per-item warnings (e.g. a branch that could not be deleted because it was not fully merged) are surfaced together as follow-up toasts. Clicking a button whose status has no matching worktrees shows a `No worktrees with status 'X' to delete.` toast instead of opening the dialog.

#### Adapts to your terminal

The dashboard inspects the available width and either renders the full table or falls into a **compact mode** that drops less-essential columns and shortens header labels (e.g. `Ahead/Behind` → `A/B`). Anything that does not fit in the grid surfaces on the footer as a `Narrow view: N hidden` indicator with the most important detail (PR title, commit summary) pulled out into a per-row detail line, so you never lose information just because the terminal is narrow.

#### Why this matters when running multiple agents

Without the dashboard, the only way to answer "which of my five agents has something interesting going on?" is to `cd` into each worktree, run `git status`, `git log -1`, and possibly `gh pr view`, then write the result down somewhere because you will forget by the time you check the fifth. The dashboard collapses that loop into a single always-fresh screen — and the row actions let you go from "I see something I want to check" to "I am in the right directory with my editor open" in one keystroke. For parallel AI workflows, that is the difference between *managing* the agents and *being managed by* them.

### Advanced Features

These capabilities are implemented in `wisetree` and are especially useful once you are managing real projects, pull requests, and multiple agent runs at the same time:

| Advanced feature | Where to use it | What it does | Why developers care |
| --- | --- | --- | --- |
| **Wise project setup presets** | Main menu → `Setup Project Config` | Scans the repository, detects known stacks, and writes a project-local `.wisetree.json` with copy patterns, ignore patterns, shared-cache links, and post-create commands. The preset catalog includes Rails, Django, FastAPI, Flask, Next.js, React, Vue/Nuxt, Angular, Svelte, Astro, Remix, Express, NestJS, Flutter, Spring Boot, .NET, Go, Rust, Laravel, Phoenix, Android, iOS, and Generic. | New projects get sensible `wisetree` defaults without hand-authoring every glob and setup command. Monorepos benefit because Wise discovery merges nested app presets into one de-duplicated config. |
| **Shared dependency cache** | `worktreeLinkPatterns`, `worktreeLinkStrategy`, `wisetree cache` | Links heavy directories such as `node_modules`, `target`, `.venv`, `vendor/bundle`, `Pods`, or `.gradle` from a per-repository cache instead of duplicating them in every worktree. Cache entries can be created empty, seeded from the source checkout, inspected, pruned, cleared, or printed as a path. | Creating worktrees stops meaning "download every dependency again"; repeated agent branches can share expensive install/build output safely. |
| **Dashboard PR control plane** | `wisetree dashboard` row actions | With `gh` available and `dashboard.showPullRequests` enabled, dashboard rows expose PR actions: open in browser, squash-merge with fetched title/body, update a PR branch from the first reachable base ref, or close the PR. Rows also show CI, review, and merge-readiness signals. | Common PR maintenance happens from the same table where you decide which worktree needs attention. |
| **AI-assisted PR conflict resolution** | Dashboard → `Update Pull Request`; configure `dashboard.useAi` | When updating a PR hits merge conflicts, `wisetree` can hand the conflicted worktree to `opencode` with a generated merge-resolution prompt, stream the embedded AI activity in the TUI, then let you complete and push or cancel and abort the merge. | PR branches can be brought up to date without leaving the dashboard, while the final commit/push decision stays under human control. |
| **AI model picker** | Settings → Dashboard → `useAi` | Fetches provider/model pairs for `opencode` from the public models catalog and can also surface locally available free `opencode` models. Selecting one writes the exact `provider/model` value used for AI conflict resolution. | Developers do not need to memorize model IDs or edit JSON by hand to enable the AI merge workflow. |
| **AI harness activity detection** | Dashboard `AI Status` column | Detects Claude Code, Opencode, Codex CLI, and Gemini CLI activity from their on-disk session/state files, then aggregates each worktree as `Pending`, `Running`, `Finished`, or `Failed`. Detection is file-based and cross-platform. | When several agents are running, the dashboard can show which worktrees are still active and which are ready for review. |
| **Safe bulk cleanup** | Dashboard footer buttons | Bulk-delete by status group (`Merged`, `Closed`, `Open`, `Clean`, `Dirty`) with a confirmation dialog, protected main checkout, optional branch deletion through `deleteBranchWithWorktree`, and per-item warnings after the run. | Cleanup becomes a deliberate batch operation instead of a risky sequence of manual `rm`, `git worktree remove`, and `git branch -d` commands. |
| **Config editor and config sync** | Settings | The TUI edits copy patterns, ignore patterns, link patterns, link strategy, cache directory, post-create commands, terminal command, path template, dashboard settings, and branch-deletion behavior. It can also copy the full config between global settings and the repo-local `.wisetree.json`. | Team defaults and personal defaults can be moved or tuned without manually editing nested JSON. |
| **Scriptable dashboard snapshots** | `wisetree dashboard --json` / `--watch` | Emits one dashboard snapshot as JSON or streams snapshots as JSON Lines. Rows include worktree, git, PR, and AI-status fields when those enrichments are enabled. | CI scripts, local automation, status bars, and custom dashboards can consume the same state the TUI uses. |
| **Shell integration and row navigation** | `Setup Shell Integration`, dashboard row actions | Installs a shell wrapper and completions so `wisetree` can change the parent shell into a selected worktree. Dashboard actions can also open the configured editor/terminal command or copy the path. | Moving from "I found the worktree" to "I am inside it and ready to work" becomes one action. |
| **Deletion safety and recovery** | Dashboard delete, bulk delete, worktree deletion service | Refuses dirty deletions unless forced, protects current/default branches, can delete the matching branch when configured, retries submodule-related worktree removal safely, unlinks shared-cache directories before removal, and falls back to manual cleanup plus `git worktree prune` for corrupted worktrees. | Destructive operations are guarded around the failure modes developers actually hit in long-running worktree-heavy repos. |

### Configuration

`wisetree` loads the **first** of these it finds; the two files are never merged:

1. `.wisetree.json` at the repo root (project-local, commit it to share with the team).
2. `~/.wisetree/settings.json` (global, your personal defaults — auto-created on first run).

Any field you omit falls back to the built-in default below, not to the global file.

A complete configuration example:

```rb
{
  "worktreeCopyPatterns": [".env*", ".vscode/**", "config/master.key"],
  "worktreeCopyIgnores": [
    "**/node_modules/**",
    "**/dist/**",
    "**/.git/**",
    "**/Thumbs.db",
    "**/.DS_Store"
  ],
  "worktreeLinkPatterns": ["node_modules", "target"],
  "worktreeLinkStrategy": "SeedIfPresent",
  "worktreeLinkCacheDir": null,
  "worktreePathTemplate": "$BASE_PATH.worktree",
  "postCreateCmd": [
    "bundle install",
    "yarn install",
    "bin/rails db:prepare"
  ],
  "terminalCommand": "code $WORKTREE_PATH",
  "deleteBranchWithWorktree": true,
  "dashboard": {
    "refreshIntervalMs": 5000,
    "showPullRequests": false,
    "columns": ["branch", "status", "ai_status", "ahead_behind", "last_commit"],
    "useAi": "anthropic/claude-sonnet-4-5",
    "aiStatus": {
      "enabledHarnesses": ["claude_code", "opencode", "codex_cli", "gemini_cli"],
      "activeWindowMs": 10000
    }
  }
}
```

The variables `$BASE_PATH`, `$WORKTREE_PATH`, `$BRANCH_NAME`, and `$SOURCE_BRANCH` are interpolated in `worktreePathTemplate`, `postCreateCmd`, and `terminalCommand` at runtime.

| Field | Type | Default | Purpose |
| --- | --- | --- | --- |
| `worktreeCopyPatterns` | `string[]` | `[".env*", ".vscode/**"]` | Glob patterns of files to copy from the source repo into a brand-new worktree. |
| `worktreeCopyIgnores` | `string[]` | `["**/node_modules/**", "**/dist/**", "**/.git/**", "**/Thumbs.db", "**/.DS_Store"]` | Glob patterns to skip during the copy step. |
| `worktreeLinkPatterns` | `string[]` | `[]` | Directory patterns to symlink into new worktrees from the shared dependency cache. |
| `worktreeLinkStrategy` | `string` | `"CreateEmpty"` | How to prepare a cache entry before linking it: `CreateEmpty`, `SeedFromSource`, or `SeedIfPresent`. |
| `worktreeLinkCacheDir` | `string \| null` | `null` | Optional override for the per-repository shared cache root. |
| `worktreePathTemplate` | `string` | `"$BASE_PATH.worktree"` | Template that decides where new worktrees live on disk, relative to the repo's parent directory. |
| `postCreateCmd` | `string[]` | `[]` | Ordered list of shell commands executed inside the new worktree, with live progress in the TUI. |
| `terminalCommand` | `string` | `""` | Optional command spawned right after creation (e.g. `code $WORKTREE_PATH`) to open an editor or terminal. |
| `deleteBranchWithWorktree` | `boolean` | `false` | When `true`, deleting a worktree also deletes its associated branch. |
| `dashboard` | `object` | see below | Live dashboard polling, visible columns, and PR enrichment settings. |

Dashboard sub-fields:

| Field | Type | Default | Purpose |
| --- | --- | --- | --- |
| `dashboard.refreshIntervalMs` | `number` | `5000` | Poll interval in milliseconds, clamped to `5000..60000` when loaded. |
| `dashboard.showPullRequests` | `boolean` | `false` | Enables `gh pr list` enrichment when the GitHub CLI is installed. |
| `dashboard.columns` | `string[]` | `["branch", "status", "ai_status", "ahead_behind", "last_commit"]` | Column order for the live dashboard table. Also supports `pull_request`. |
| `dashboard.useAi` | `string` | `""` | Provider/model passed to `opencode run -m` for AI-assisted PR conflict resolution. Blank disables AI conflict resolution. |
| `dashboard.aiStatus.enabledHarnesses` | `string[]` | `["claude_code", "opencode", "codex_cli", "gemini_cli"]` | AI harnesses included in the dashboard's `AI Status` column. |
| `dashboard.aiStatus.activeWindowMs` | `number` | `10000` | File-write recency threshold for reporting an AI harness as `Running`, clamped to `2000..60000`. |

# 📟 Wisetree CLI

The full surface of the binary, suitable for both humans and scripts. Every subcommand has an interactive equivalent, but the flags below let you skip the TUI entirely.

### Top-level invocation

```rb
wisetree [command] [options]
```

### Commands

| Command | Description |
| --- | --- |
| `(no command)` | Open the interactive main menu. |
| `create` | Create a new worktree (interactive unless flags are supplied). |
| `dashboard` | Open the live dashboard, print one JSON snapshot, or stream JSON Lines. |
| `cache list` | Show shared dependency cache entries, size, and active worktree users. |
| `cache prune` | Remove orphaned shared cache entries older than 14 days. |
| `cache clear` | Delete this repository's shared cache. Requires `--force`. |
| `cache path` | Print the shared cache root path. |
| `settings` | Open the settings screen to inspect and edit configuration. |

### Global / interactive options

| Flag | Alias | Description |
| --- | --- | --- |
| `--help` | `-h` | Show the built-in help screen. |
| `--version` | `-v` | Print the installed version. |
| `--mode <mode>` | `-m` | Land directly on a specific screen (`menu`, `create`, `dashboard`, `cache`, `settings`). |
| `--from-wrapper` | `None` | Used internally by the shell wrapper so `wisetree` can print the selected path on stdout for the parent shell to `cd` into. |

### Non-interactive options

| Flag | Alias | Applies to | Description |
| --- | --- | --- | --- |
| `--name <name>` | `-n` | `create` | Worktree directory name. |
| `--source <branch>` | `-s` | `create` | Source branch to fork the new worktree from. |
| `--branch <branch>` | `-b` | `create` | New branch name; defaults to the directory name. |
| `--force` | `-f` | `cache clear` | Confirm destructive cache clearing in non-interactive mode. |
| `--json` | `None` | `dashboard` | Emit JSON suitable for piping into `jq`. |
| `--watch` | `-w` | `dashboard` | Stream dashboard snapshots as JSON Lines until `Ctrl+C`. |

### Interactive examples

```rb
wisetree                # Open the main menu
wisetree create         # Jump straight into the create flow
wisetree dashboard      # Open the live dashboard
wisetree cache          # Open the shared cache screen
wisetree settings       # Open the settings screen
```

### Non-interactive examples

```rb
wisetree create -n my-feature -s main                  # Create a worktree off main
wisetree create -n my-feature -s main -b feat/payments # Create with an explicit new branch
wisetree dashboard --json                              # Print one dashboard snapshot as JSON
wisetree dashboard --watch                             # Stream dashboard snapshots as JSON Lines
wisetree cache list --json                             # Print shared cache details as JSON
wisetree cache clear --force                           # Remove this repo's shared cache
```

### Update checks

`wisetree` lazily checks the npm registry at most once every 24 hours and surfaces a banner inside the TUI when a newer version is available. The cache lives at `~/.wisetree/state.json` so the check never gets in your way.

# 🤝 Contribute

Contributions of every shape are welcome, including bug reports, feature requests, design feedback on the palette, documentation polish, and code. The repository follows a few simple conventions:

1. **Fork and branch** off `main`. Use a descriptive branch name (e.g. `feat/json-list-flag`, `fix/shell-wrapper-zsh`).
2. **Run the test suite** before opening a PR:

```rb
cargo test --all
```

3. **Keep the formatter and linter happy:**

```rb
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

4. **Open a Pull Request** against `main` using the template in `.github/pull_request_template.md`. Include a short description, a screenshot or screencast for any TUI change, and a checklist of how to test it manually.

The CI pipeline (`.github/workflows/ci.yml`) runs the tests, formatter, and clippy on every push, and the release pipeline (`release.yml`, generated by `cargo-dist`) ships the binaries to GitHub Releases, npm, and the Homebrew tap.

# 📦 Publishing a release

`wisetree` ships to GitHub Releases, the npm registry, and the Homebrew tap on every signed git tag, driven by [`cargo-dist`](https://github.com/axodotdev/cargo-dist). The flow below is for maintainers cutting a new version.

### Per-release flow

1. **Bump the version.** Update `Cargo.toml` plus the six `npm/wisetree*/package.json` files (the umbrella `wisetree` package and the five platform packages) so they match.
2. **Tag and push:**

```rb
git tag -a v1.x.y -m "v1.x.y"
git push origin v1.x.y
```

3. **`release.yml` takes over.** It builds artifacts for `aarch64`/`x86_64` macOS and Linux plus `x86_64` Windows, attaches them to a new GitHub Release, publishes the umbrella + platform packages to npm, and pushes an updated formula to `victorcorcos/homebrew-tap` (replacing the `REPLACE_WITH_RELEASE_SHA` placeholders in `homebrew-tap/Formula/wisetree.rb`).

After the workflow finishes, both `brew install victorcorcos/tap/wisetree` and `npm install -g wisetree` will resolve to the new version.

### One-time setup

The pipeline expects two secrets under the repo's **Settings → Secrets and variables → Actions**:

| Secret | Purpose |
| --- | --- |
| `NPM_TOKEN` | An [npm automation token](https://docs.npmjs.com/creating-and-viewing-access-tokens) with publish rights to `wisetree` and the five `wisetree-<platform>` packages. |
| `HOMEBREW_TAP_TOKEN` | A GitHub PAT with `contents: write` on `victorcorcos/homebrew-tap`. |

Before the very first release, also reserve the package names on npm:

```rb
npm view wisetree              # confirm the name is free (or owned by you)
npm view wisetree-darwin-arm64 # repeat for each platform package
```

If any name is taken, rename consistently across `Cargo.toml`, the six `npm/` packages, the homebrew formula, and the install scripts.

### Homebrew core vs. the tap

`brew install victorcorcos/tap/wisetree` works the moment the tag publishes. Promoting `wisetree` into `homebrew/core` (so users can run `brew install wisetree` directly) requires clearing the [notability bar](https://docs.brew.sh/Acceptable-Formulae) — roughly 75+ GitHub stars, 30+ forks, 30+ days of stable history, and no other comparable install path. Until then, the tap is the supported channel.

# 🪪 License

`wisetree` is released under the **MIT License**. See [`LICENSE`](./LICENSE) for the full text. In short: do whatever you want with it, just keep the copyright notice intact.

# 👤 Creator

Victor Cordeiro Costa

1. Github: https://github.com/victorcorcos
2. Linkedin: https://www.linkedin.com/in/victorcorcos/

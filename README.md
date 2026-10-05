# Wisetree

<div align="center">
  <img src="https://i.imgur.com/vO0AOis.gif" alt="Wisetree" width="50%" />
</div>

**Wisetree** is a terminal UI for managing [Git worktrees](https://git-scm.com/docs/git-worktree), built with Rust and [Ratatui](https://ratatui.rs). It lets you work on several branches at the same time, each in its own directory, without stashing, re-installing dependencies, or losing track of what is where.

With Wisetree you can:

- **Create** a worktree from any branch, tag, or commit. Wisetree copies your untracked config files (`.env`, editor settings, and so on), shares heavy dependency folders, and runs your setup commands.
- **Watch** every worktree on a live **Dashboard** that shows Git status, ahead/behind counts, diff size, last commit, and GitHub pull request state.
- **Act** on a worktree from the Dashboard: jump into it, open it in your editor, copy its path, delete it, or open, close, or squash-merge its pull request.
- **Clean up** in bulk, for example by deleting every worktree whose PR was merged.
- **Bootstrap** a project configuration from presets for more than 20 frameworks, or let Wisetree detect them.
- **Script** it: `create`, `dashboard`, and `cache` also work non-interactively, with JSON output.

---

## Table of contents

- [Requirements](#requirements)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Shell integration](#shell-integration)
- [Interactive mode](#interactive-mode)
  - [Main menu](#main-menu)
  - [Create a worktree](#create-a-worktree)
  - [Dashboard](#dashboard)
  - [Deleting worktrees](#deleting-worktrees)
  - [Pull request actions](#pull-request-actions)
  - [Shared cache screen](#shared-cache-screen)
  - [Settings](#settings)
  - [Setup Project Config (presets)](#setup-project-config-presets)
- [Command-line reference](#command-line-reference)
- [Configuration](#configuration)
  - [Where configuration lives](#where-configuration-lives)
  - [Full example](#full-example)
  - [Configuration keys](#configuration-keys)
  - [Template variables](#template-variables)
  - [Worktree location (`worktreePathTemplate`)](#worktree-location-worktreepathtemplate)
  - [Copying files (`worktreeCopyPatterns`)](#copying-files-worktreecopypatterns)
  - [Shared dependency cache (`worktreeLinkPatterns`)](#shared-dependency-cache-worktreelinkpatterns)
  - [Post-create commands (`postCreateCmd`)](#post-create-commands-postcreatecmd)
  - [Terminal command (`terminalCommand`)](#terminal-command-terminalcommand)
  - [Dashboard settings (`dashboard`)](#dashboard-settings-dashboard)
- [Safety guarantees](#safety-guarantees)
- [Keyboard and mouse reference](#keyboard-and-mouse-reference)
- [Troubleshooting](#troubleshooting)
- [Development](#development)
- [License](#license)

---

## Requirements

| Requirement | Needed for |
| --- | --- |
| **Git** | Everything. Wisetree runs `git` as a subprocess. |
| **Rust (stable toolchain)** | Building from source. `rust-toolchain.toml` pins the stable channel. |
| **GitHub CLI (`gh`)**, authenticated with `gh auth login` | Optional. Needed only for pull request information and actions. |
| **zsh or bash** | Optional. Needed only for [shell integration](#shell-integration). |
| **A clipboard tool**: `pbcopy` (macOS), `wl-copy` or `xclip` (Linux), `clip` (Windows) | Optional. Needed only for "Copy path to clipboard" and mouse text selection. |

Wisetree is tested in CI on **Ubuntu** and **macOS**.

## Installation

```bash
git clone https://github.com/victorcorcos/wisetree.git
cd wisetree
cargo build --release
export PATH="$PWD/target/release:$PATH"   # add this line to your shell profile to keep it
```

Check the installation:

```bash
wisetree --version   # prints "Wisetree v1.0.0"
```

## Quick start

```bash
cd path/to/your/repo
wisetree
```

1. *(Recommended)* Choose **Setup Shell Integration** so Wisetree can move your shell into worktrees. Reload your shell afterward.
2. Open **Settings → Setup Project Config** and pick **Wise Preset**. Wisetree scans the repository and suggests which files to copy, which folders to share, and which setup commands to run.
3. Choose **Create**, type a directory name, pick a source branch, and type a new branch name.
4. Open the **Dashboard** to see all of your worktrees, then press **Enter** on one to act on it.

By default, a worktree for repository `~/code/my-app` named `feature-x` is created at:

```text
~/code/my-app.worktree/feature-x
```

You can change this location with [`worktreePathTemplate`](#worktree-location-worktreepathtemplate).

---

## Shell integration

A child process cannot change its parent shell's directory, so Wisetree needs a small shell function to `cd` you into a worktree.

**To install it:** run `wisetree` and choose **Setup Shell Integration**. The entry is marked *recommended* and appears only while the integration is not installed. Pick your shell (the detected one is preselected), review the preview, and confirm.

Wisetree appends a block delimited by `# Wisetree setup: added on <date>` and `# End Wisetree setup` to:

| Shell | File |
| --- | --- |
| zsh | `~/.zshrc` |
| bash on Linux | `~/.bashrc` |
| bash on macOS | `~/.bash_profile` |

The block adds tab completion and a wrapper function:

```bash
wisetree() {
  if [ $# -eq 0 ]; then
    local dir
    if dir=$(FORCE_COLOR=3 command wisetree --from-wrapper); then
      if [ -n "$dir" ]; then
        builtin cd "$dir" && echo "Wisetree: Navigated to $(pwd)"
      fi
    fi
  else
    command wisetree "$@"
  fi
}
```

**After you reload your shell:**

- `wisetree` with no arguments opens the TUI. When you choose a worktree with **Navigate to Directory** on the Dashboard, or answer **Yes** to "Navigate to Worktree" after creating one, your shell moves into it when Wisetree exits.
- `wisetree <anything>` runs the binary unchanged.

In wrapper mode the TUI draws on `/dev/tty`, and only the selected path is written to stdout. Paths that contain newlines or control characters are refused so that they cannot inject shell commands.

---

## Interactive mode

Run `wisetree` inside any Git repository, or inside any of its worktrees. Interactive mode requires a TTY.

### Main menu

The header shows the Wisetree version and the active repository.

| Entry | When it appears | What it does |
| --- | --- | --- |
| **Setup Shell Integration** | When the integration is not installed for zsh or bash | Installs the [shell wrapper](#shell-integration). |
| **Create** | Always | Creates a new worktree. |
| **Dashboard** | Always | Opens the live worktree overview. |
| **Shared cache** | When `worktreeLinkPatterns` is not empty | Inspects and cleans the [shared dependency cache](#shared-dependency-cache-worktreelinkpatterns). |
| **Settings** | Always | Views and edits the configuration. |
| **Exit** | Always | Quits. `Ctrl+C` quits from any screen. |

You can open a screen directly with `wisetree create`, `wisetree dashboard`, `wisetree cache`, or `wisetree settings`. You can also use `--mode <menu|create|dashboard|cache|settings>`.

### Create a worktree

The Create screen is a step-by-step wizard:

1. **Directory name.** This is the folder name for the worktree. It cannot be empty, contain `/` or `\`, start with `.` or `-`, contain `< > : " | ? *` or control characters, or be longer than 255 characters.
2. **Source branch.** This is a searchable list of local and remote branches. `upstream/main`, `upstream/master`, `origin/main`, and `origin/master` come first, followed by the remaining branches sorted by most recent commit. Choose **Enter custom ref** to type a tag, commit SHA, or any other ref.
3. **New branch name.** Wisetree creates this branch from the source with `git worktree add -b`. Whitespace becomes `_`, and the name must be a valid Git branch name. **Leave it blank** to check out the source branch itself instead of creating a new branch. Wisetree refuses names of branches that already exist.
4. **Confirmation.** Review a summary of the directory, source, branch, and final path.
5. **Navigate to Worktree?** Choose whether your shell should move into the new worktree when Wisetree exits. This requires [shell integration](#shell-integration).
6. **Creation.** A **Terminal Activity** panel streams each step live:
   - `git worktree add …`
   - the files copied by [`worktreeCopyPatterns`](#copying-files-worktreecopypatterns)
   - the directories linked by [`worktreeLinkPatterns`](#shared-dependency-cache-worktreelinkpatterns)
   - the stdout and stderr of every [post-create command](#post-create-commands-postcreatecmd), cleaned of ANSI escape sequences
7. **Launch.** If [`terminalCommand`](#terminal-command-terminalcommand) is set, Wisetree runs it to open your editor or terminal in the new worktree.

`Esc` goes back one step.

### Dashboard

The Dashboard is a live table of every worktree in the repository.

```text
Refreshed 2s ago - 5 worktrees, 1 dirty, 2 PRs open

Search: █

Worktree        Branch            Status           Ahead/Behind  Last Commit
my-app          main              Mother           =0            81a3742 Enhance README
feature-x       feat/feature-x    Opened 🟢 👍 ✅   +3 -0         4b1c9e2 Add filters
old-fix         fix/typo          Merged           +1 -12        9f0a1d3 Fix typo
spike           spike/cache       Dirty            +2 -4         c0ffee1 WIP
...
                          Delete worktrees with status: [Merged] [Closed] [Drafted] [Opened] [Clean] [Dirty]
```

#### Status bar and refresh

- The top line shows when the data was last refreshed and how many worktrees, dirty worktrees, and open PRs there are.
- Git data is polled every `dashboard.refreshIntervalMs`. The default is 5 seconds, and the allowed range is 5–60 seconds.
- Pull request data is refreshed every 30 seconds. On that same cycle Wisetree also fetches the base branch, so that the *behind* count includes commits that teammates pushed. PR results are cached in `~/.wisetree/dashboard_pr_cache.json`, which means the table appears instantly, and Wisetree backs off for 5 minutes if GitHub rate-limits it.
- `Ctrl+R` forces a refresh.

#### Search

Start typing at any time to filter the table with fuzzy matching. `Esc` clears the search, and pressing `Esc` again leaves the Dashboard.

#### Columns

The *Worktree* column, which shows the directory name, is always visible. You choose the remaining columns with `dashboard.columns`. Columns are shortened automatically on narrow terminals.

| Column key | Header | Shows |
| --- | --- | --- |
| `branch` | Branch | Checked-out branch. |
| `status` | Status | Status label, plus pull request indicators (see below). |
| `ahead_behind` | Ahead/Behind | `+N` commits ahead and `-N` commits behind the base ref. `=0` means the worktree is in sync. |
| `diff` | Diff | `+N -N` lines inserted and deleted relative to the base ref. |
| `last_commit` | Last Commit | Short SHA and subject of `HEAD`. |
| `pull_request` | PR | `#123 Open`, `Merged`, `Closed`, or `Draft`. Requires PR data. |

Wisetree picks the **base ref** for ahead/behind and diff in this order:

1. The PR's base branch on `upstream`, then on `origin`.
2. The branch's tracked upstream, unless that upstream is just its own pushed copy (`origin/<same-branch>`).
3. The first reachable ref among `upstream/main`, `upstream/master`, `upstream/develop`, `origin/main`, `origin/master`, and `origin/develop`.

#### Status labels

| Label | Meaning |
| --- | --- |
| **Mother** | The main worktree. It cannot be deleted. |
| **Merged** | The branch's PR was merged. |
| **Opened** | The branch has an open PR. |
| **Drafted** | The branch has a draft PR. |
| **Closed** | The branch's PR was closed without merging. |
| **Clean** | No PR, and no uncommitted changes. |
| **Dirty** | No PR, and there are uncommitted changes. |

For **Opened** PRs, up to three indicators follow the label:

| CI checks | Review | Mergeability |
| --- | --- | --- |
| ⚪ pending | ✋ review requested | ✅ ready to merge |
| 🟡 running | 👍 approved | 🔄 behind base |
| 🟢 passed | 👎 changes requested | ❌ conflicts |
| 🔴 failed | | 🚫 blocked |
| ⚠️ errored | | ⏳ waiting on hooks |
| | | 🏚️ unstable checks |
| | | 📝 draft |
| | | ❓ unknown |

PR data is shown only when `dashboard.showPullRequests` is `true` and `gh` is installed. If `gh` is missing, the PR column is hidden and a warning is shown.

#### Action menu

Press **Enter** on a row to open its action menu. **General Commands** is a searchable list:

| Action | When it appears |
| --- | --- |
| **Navigate to Directory** | When Wisetree was launched through [shell integration](#shell-integration). Exits and moves your shell into the worktree. |
| **Open with Command** | When `terminalCommand` is set. Runs it for this worktree. |
| **Copy path to clipboard** | When a clipboard tool is available. |
| **Delete worktree** | For any worktree except the main one. |

**Pull Request Commands** are buttons. Press `Tab` to move between them and the list, or use a letter shortcut:

| Button | Shortcut | When it appears |
| --- | --- | --- |
| **Open** | `O` | The worktree's branch has a PR. Opens it in your browser. |
| **Merge** | `M` | The PR is open. See [merging](#merge-squash). |
| **Close** | `C` | The PR is open or a draft. Asks for confirmation before closing. |

#### Quick delete and bulk delete

- With the search box empty, **Backspace** or **Delete** on a row jumps straight to the delete confirmation for that worktree.
- Press **Tab** to focus the footer row **Delete worktrees with status: Merged · Closed · Drafted · Opened · Clean · Dirty**. Use **← / →** to pick a status and press **Enter**. Wisetree lists every worktree with that status and asks you to confirm. The main worktree is never included.

### Deleting worktrees

Every deletion requires confirmation.

- A **dirty** worktree has uncommitted changes. Its dialog is red and the button reads **Force Delete**. Its changes are discarded.
- When [`deleteBranchWithWorktree`](#configuration-keys) is `true`, the dialog warns that the branch will be deleted too. A clean worktree's branch is deleted with `git branch -d`, so a branch with unmerged commits is **kept**, and Wisetree tells you why. A force-deleted worktree's branch is deleted with `git branch -D`.
- Shared-cache symlinks are removed before the worktree, so cached dependencies are never deleted through a link.
- If Git reports the worktree as corrupted, Wisetree removes the directory and runs `git worktree prune`.

### Pull request actions

Wisetree works with **pull requests that already exist** on GitHub. It does not create them. All actions run through `gh` in the repository.

#### Open

Opens the PR URL in your default browser with `open`, `xdg-open`, or `start`. Only `http` and `https` URLs are opened.

#### Close

Runs `gh pr close <number>` after a confirmation dialog.

#### Merge (squash)

1. Wisetree loads the PR title and description and shows a summary: the worktree, branch, CI status, ahead/behind counts, last commit, and a preview of the **squash commit message**.
2. The confirmation defaults to **No**.
3. If the worktree has **local commits that are not pushed**, Wisetree warns you, because a squash merge includes only what GitHub already has. It then asks:
   - **Push & merge** (default): runs `git push origin HEAD`, then merges.
   - **Merge only**: merges what is already on GitHub.
4. Wisetree runs `gh pr merge <number> --squash --subject "<PR title> (#<number>)" --body "<PR description>"`. The resulting commit message matches GitHub's own squash convention.

### Shared cache screen

This screen is available when [`worktreeLinkPatterns`](#shared-dependency-cache-worktreelinkpatterns) is configured. It lists every cached directory with its size, its age, and the worktrees that use it.

| Key | Action |
| --- | --- |
| `R` | Refresh. |
| `D` | Delete the selected cache entry. The confirmation warns if worktrees still reference it. |
| `Esc` | Back. |

For pruning, clearing, and JSON output, use [`wisetree cache`](#cache).

### Settings

Settings shows every configuration value and lets you edit it in place. Each page shows the file that changes (**Saving to: …**): the project config if one exists, otherwise the global config.

| Entry | What you can do |
| --- | --- |
| **Setup Project Config** | Shown when the project has no `.wisetree/config.json`. See [presets](#setup-project-config-presets). |
| **Dashboard** | Edit the refresh interval, toggle PR data, and choose columns. |
| **Copy Patterns** / **Ignore Patterns** / **Link Patterns** | Edit the patterns in a multi-line editor with one pattern per line. |
| **Link Strategy** | Cycle through `CreateEmpty`, `SeedFromSource`, and `SeedIfPresent`. |
| **Link Cache Dir** | Override the shared-cache root. |
| **Post-Create Commands** | Add, edit, and remove commands. They run in order. |
| **Terminal Command** | Set the command that opens your editor or terminal. |
| **Path Template** | Set where worktrees are created. |
| **Copy Settings** | Copy the whole file **global → local** or **local → global**. |
| **Delete Branch with Worktree** | Toggle `deleteBranchWithWorktree`. |

Text inputs support common editing shortcuts: `Ctrl`/`Alt` + arrows move by word, `Ctrl+W` or `Alt+Backspace` deletes a word, and `Ctrl+U` / `Ctrl+K` delete to the start or end of the line. Paste is supported.

If a configuration file contains invalid JSON, Wisetree shows the file path and the parse error. You can fix the file by hand, or press `R` to rewrite the global `~/.wisetree/config.json` with the defaults.

### Setup Project Config (presets)

This flow creates `.wisetree/config.json` for the current project.

1. **Pick a preset.**
   - **Wise Preset** (default) scans the repository recursively, including monorepo subfolders, while skipping dependency, build, and cache directories. It detects every framework it recognizes and merges their settings, scoped to the right subfolders.
   - Or pick a single preset. A preset whose files are found at the repository root is labeled *detected*.
2. **Review and edit** four blocks: Copy Patterns, Ignore Patterns, Shared Cache Links, and Post-Create Commands. Each line is one entry.
3. **Confirm** to write the file. If link patterns are present, the link strategy is set to `SeedFromSource`.

Available presets:

| Category | Presets |
| --- | --- |
| Ruby, Python, PHP, Elixir | Ruby on Rails, Django, FastAPI, Flask, Laravel, Phoenix |
| JavaScript and TypeScript | Next.js, Remix, NestJS, Vue / Nuxt, Angular, Svelte / SvelteKit, Astro, React (CRA / Vite), Express / Node.js |
| Mobile | Flutter / Dart, Android (Gradle), iOS / Xcode (Swift) |
| JVM, .NET, Go, Rust | Spring Boot (Maven), Spring Boot (Gradle), .NET / ASP.NET Core, Go, Rust / Cargo |
| Fallback | Generic |

For example, the **Ruby on Rails** preset:

- copies `.env*`, `config/master.key`, `config/database.yml`, and similar files
- ignores `tmp/`, `log/`, `node_modules/`, and similar directories
- shares dependency folders through the cache
- runs `bundle install`, `yarn install`, and `bin/rails db:prepare`

---

## Command-line reference

```text
wisetree [command] [options]
```

### Commands

| Command | Without flags | With flags |
| --- | --- | --- |
| *(none)* | Opens the main menu. | — |
| `create` | Opens the Create screen. | Creates a worktree non-interactively. |
| `dashboard` | Opens the Dashboard. | Prints worktree data as JSON. |
| `cache` | Opens the Shared cache screen. | `cache list`, `cache prune`, `cache clear`, and `cache path` are always non-interactive. |
| `settings` | Opens Settings. | — |

### Options

| Option | Applies to | Description |
| --- | --- | --- |
| `-h`, `--help` | all | Shows help. |
| `-v`, `--version` | all | Shows the version. |
| `-m`, `--mode <mode>` | interactive | Initial screen: `menu`, `create`, `dashboard`, `cache`, or `settings`. |
| `--from-wrapper` | interactive | Used by the shell wrapper. Prints the selected path to stdout. |
| `-n`, `--name <name>` | `create` | **Required.** Worktree directory name. |
| `-s`, `--source <branch>` | `create` | **Required.** Existing source branch, local or remote. |
| `-b`, `--branch <branch>` | `create` | Name of the new branch. Defaults to the value of `--name`. |
| `--json` | `dashboard`, `cache list` | Prints JSON. |
| `-w`, `--watch` | `dashboard` | Streams snapshots as JSON Lines until you press `Ctrl+C`. |
| `-f`, `--force` | `cache clear` | Confirms deletion of the cache. |

Values can be given as `--name foo` or `--name=foo`. Unknown flags are errors.

### `create`

```bash
wisetree create -n feature-x -s main                 # creates branch "feature-x" from main
wisetree create -n feature-x -s origin/main -b feat/x
```

The non-interactive `create` runs the same pipeline as the TUI, including copy, link, post-create commands, and terminal command. It prints the worktree path, then the source and branch:

```text
/Users/me/code/my-app.worktree/feature-x
  source: main
  branch: feature-x
```

### `dashboard`

```bash
wisetree dashboard --json      # one pretty-printed JSON array
wisetree dashboard --watch     # one compact JSON array per line, on every refresh
```

Example output:

```json
[
  {
    "path": "/Users/me/code/my-app",
    "branch": "main",
    "commit": "81a3742d5da78d3a3bbfadef44b42be02d6a80dd",
    "isMain": true,
    "isClean": true,
    "branchStatus": {
      "ahead": 0,
      "behind": 0,
      "upstreamBranch": "origin/main",
      "insertions": 0,
      "deletions": 0
    },
    "lastCommit": {
      "sha": "81a3742",
      "summary": "Enhance README with image and formatting",
      "relativeTime": "3 minutes ago",
      "author": "Jane Doe"
    }
  }
]
```

When PR data is enabled, each row also has a `pullRequest` object with `number`, `state`, `url`, `title`, and, when available, the base branch, labels, checks, review, and merge status.

### `cache`

| Command | Description |
| --- | --- |
| `wisetree cache list [--json]` | Shows the cache root, total size, active worktrees, and each entry's size, age in days, and number of users. |
| `wisetree cache prune` | Removes entries that **no active worktree uses** and that were **last used more than 14 days ago**. Reports what was removed and what was kept, and why. |
| `wisetree cache clear --force` | Deletes this repository's entire cache. Refuses to run without `--force`. |
| `wisetree cache path` | Prints the cache root, for example `~/.wisetree/cache/34e728ff50f6577c`. |

Non-interactive commands print errors to stderr and exit with status `1`.

---

## Configuration

### Where configuration lives

| Scope | Path | Notes |
| --- | --- | --- |
| Project | `<main worktree>/.wisetree/config.json` | Shared by every worktree of the repository. |
| Project (fallback) | `<current worktree>/.wisetree/config.json` | Used only if the main worktree has no project config. |
| Global | `~/.wisetree/config.json` | Created with the defaults on first run. |

Wisetree uses the **first file that exists** in the order above. Files are **not merged**: the chosen file is the whole configuration, and missing keys take their default values.

> [!IMPORTANT]
> Keep `.wisetree/` out of version control, for example by adding it to `.gitignore` or `.git/info/exclude`. It can contain machine-specific paths and commands.

Other files in `~/.wisetree/`:

| Path | Contents |
| --- | --- |
| `cache/<repo-id>/` | Default shared dependency cache for each repository. `<repo-id>` is a hash of the repository path. |
| `dashboard_pr_cache.json` | Cached pull request data for the Dashboard. |

A JSON Schema for the configuration is in [`schema.json`](schema.json). Keys use camelCase. Unknown keys are ignored when the file is loaded and are dropped when Settings saves it.

### Full example

```json
{
  "worktreePathTemplate": "$BASE_PATH.worktree",
  "worktreeCopyPatterns": [".env*", ".vscode/**", "config/master.key"],
  "worktreeCopyIgnores": ["**/node_modules/**", "**/dist/**", "**/.git/**", "**/.DS_Store"],
  "worktreeLinkPatterns": ["node_modules"],
  "worktreeLinkStrategy": "SeedFromSource",
  "worktreeLinkCacheDir": null,
  "postCreateCmd": ["npm install", "echo Ready: $BRANCH_NAME"],
  "terminalCommand": "code $WORKTREE_PATH",
  "deleteBranchWithWorktree": true,
  "dashboard": {
    "refreshIntervalMs": 5000,
    "showPullRequests": true,
    "columns": ["branch", "status", "ahead_behind", "diff", "last_commit", "pull_request"]
  }
}
```

### Configuration keys

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `worktreePathTemplate` | string | `"$BASE_PATH.worktree"` | Folder, relative to the repository's parent directory, where worktrees are created. |
| `worktreeCopyPatterns` | string[] | `[".env*", ".vscode/**"]` | Globs of files copied from the main worktree into each new worktree. |
| `worktreeCopyIgnores` | string[] | `["**/node_modules/**", "**/dist/**", "**/.git/**", "**/Thumbs.db", "**/.DS_Store"]` | Globs that are never copied. |
| `worktreeLinkPatterns` | string[] | `[]` | Directories shared through the cache with symlinks instead of being duplicated. |
| `worktreeLinkStrategy` | `"CreateEmpty"` \| `"SeedFromSource"` \| `"SeedIfPresent"` | `"CreateEmpty"` | How a cache entry is created the first time it is needed. |
| `worktreeLinkCacheDir` | string \| null | `null` | Overrides the cache root. Supports template variables. |
| `postCreateCmd` | string[] | `[]` | Shell commands run in order inside the new worktree. |
| `terminalCommand` | string | `""` | Command that opens an editor or terminal in a worktree. Empty disables it. |
| `deleteBranchWithWorktree` | boolean | `false` | Also deletes the branch when a worktree is deleted. |
| `dashboard.refreshIntervalMs` | integer | `5000` | Git polling interval, clamped to 5000–60000. |
| `dashboard.showPullRequests` | boolean | `false` | Fetches PR data through `gh`. Required for PR columns, status indicators, and PR actions. |
| `dashboard.columns` | string[] | `["branch", "status", "ahead_behind", "last_commit"]` | Any of `branch`, `status`, `ahead_behind`, `diff`, `last_commit`, `pull_request`. Unknown names are ignored. If no valid name remains, the defaults are used. |

### Template variables

`worktreePathTemplate`, `worktreeLinkCacheDir`, `postCreateCmd`, and `terminalCommand` can use these variables:

| Variable | Value |
| --- | --- |
| `$BASE_PATH` | Name of the repository directory, for example `my-app`. |
| `$WORKTREE_PATH` | Absolute path of the worktree. |
| `$BRANCH_NAME` | Name of the new branch. |
| `$SOURCE_BRANCH` | Source branch or ref. |

In `postCreateCmd` and `terminalCommand`, each substituted value is **shell-quoted**, so branch names cannot inject commands.

### Worktree location (`worktreePathTemplate`)

The worktree path is built as follows:

```text
<parent of main worktree> / <resolved template> / <directory name>
```

| Template | Repository at `~/code/my-app`, name `feature-x` |
| --- | --- |
| `$BASE_PATH.worktree` (default) | `~/code/my-app.worktree/feature-x` |
| `worktrees/$BASE_PATH` | `~/code/worktrees/my-app/feature-x` |
| `$BASE_PATH-wt/$SOURCE_BRANCH` | `~/code/my-app-wt/main/feature-x` |

The resolved template must be a **relative** path, and it cannot contain `..`.

### Copying files (`worktreeCopyPatterns`)

Untracked files such as `.env` or local credentials do not exist in a new worktree. Wisetree copies them from the main worktree.

- Patterns are globs that also match dotfiles. A bare pattern also matches at any depth: `.env*` matches `.env` and `apps/api/.env.local`.
- Matched directories are copied recursively, and `worktreeCopyIgnores` still applies inside them.
- Errors are reported in the activity log and do not stop worktree creation.

### Shared dependency cache (`worktreeLinkPatterns`)

Installing `node_modules`, `vendor/bundle`, or `.venv` separately in every worktree is slow and uses a lot of disk space. Link patterns let all worktrees share one copy.

**How it works:**

1. For each pattern, which must be a **directory** such as `node_modules`, Wisetree keeps one entry at `<cache root>/entries/<pattern>`.
2. In the new worktree, it creates a **symlink** from `<pattern>` to that cache entry. On Windows, it uses a junction when symlinks are not permitted.
3. It records in `<cache root>/metadata.json` which worktrees use the cache, and when each entry was last used.

**`worktreeLinkStrategy`** controls what happens the first time a cache entry is created:

| Strategy | Behavior when the cache entry does not exist yet |
| --- | --- |
| `CreateEmpty` (default) | Creates an empty directory. Your post-create command, for example `npm install`, fills it. |
| `SeedFromSource` | Copies the directory from the main worktree into the cache. If the directory is missing there, creates an empty entry. |
| `SeedIfPresent` | Copies the directory from the main worktree if it exists. Otherwise skips linking that pattern. |

After an entry exists, every later worktree links to it directly.

**Cache root:** the default is `~/.wisetree/cache/<repo-id>/`. Set `worktreeLinkCacheDir` to use another location.

**Cleanup:**

- Deleting a worktree removes its symlinks first and unregisters it from the cache. The cached data stays for the other worktrees.
- To clean the cache, use `wisetree cache prune`, the [Shared cache screen](#shared-cache-screen), or `wisetree cache clear --force`.

> [!NOTE]
> All linked worktrees share the same files. Use this for dependency folders that are compatible across your branches. Don't use it for build output that differs per branch.

### Post-create commands (`postCreateCmd`)

- Commands run **in order**, in the new worktree directory.
- They run in your login shell (`$SHELL -l -c` for bash, zsh, fish, and similar shells, otherwise `/bin/sh -c`; `cmd /C` on Windows). This means your `PATH`, version managers, and aliases are available.
- Output streams live into the Terminal Activity panel.
- A failing command is reported, and the remaining commands still run.
- Commands are stopped if Wisetree exits.

```json
"postCreateCmd": [
  "npm install",
  "cp .env.example .env.local",
  "echo Created $BRANCH_NAME from $SOURCE_BRANCH"
]
```

### Terminal command (`terminalCommand`)

This command runs, detached, after a worktree is created. It also runs when you choose **Open with Command** on the Dashboard.

```json
"terminalCommand": "code $WORKTREE_PATH"
```

Other examples are `cursor $WORKTREE_PATH`, `idea $WORKTREE_PATH`, and `open -a iTerm $WORKTREE_PATH`.

### Dashboard settings (`dashboard`)

See the [Dashboard](#dashboard) section and the [configuration keys](#configuration-keys) table. To enable pull request features:

```json
"dashboard": { "showPullRequests": true, "columns": ["branch", "status", "ahead_behind", "pull_request"] }
```

Also make sure that `gh auth status` succeeds.

---

## Safety guarantees

- **The main worktree cannot be deleted.** Its row is labeled *Mother*, it has no delete action, and bulk delete never includes it.
- **Destructive actions always ask first.** This covers single deletes, bulk deletes, closing a PR, merging a PR (default **No**), deleting a cache entry, and `cache clear` (requires `--force`).
- **Dirty worktrees are flagged.** Deleting one is shown in red as a *Force Delete*.
- **Unmerged branches are kept.** Unless the deletion was forced, a branch is deleted with `git branch -d`, which refuses to delete a branch with unmerged commits.
- **Unpushed commits are not lost on merge.** You are offered **Push & merge** before squash-merging.
- **Input is validated.** Directory names, branch names, and refs are validated, and shell metacharacters are rejected. Template values are shell-quoted.
- **Only web URLs are opened.** The browser opener accepts only `http` and `https` URLs.
- **Concurrent Git operations are retried.** When Git reports a lock conflict, the operation is retried automatically.

---

## Keyboard and mouse reference

| Context | Keys |
| --- | --- |
| Everywhere | `↑` `↓` navigate · `Enter` select · `Esc` back or cancel · `Ctrl+C` quit |
| Lists | Type to search, on searchable lists |
| Dialogs | `←` `→` or `Tab` switch buttons · `Enter` confirm · `Esc` cancel |
| Dashboard | Type to search · `Ctrl+R` refresh · `Enter` action menu · `Backspace`/`Delete` delete the selected worktree (when the search is empty) · `Tab` focus the bulk-delete buttons |
| Dashboard action menu | `Tab` switch between General and PR commands · `O` open PR · `M` merge PR · `C` close PR |
| Shared cache | `R` refresh · `D` delete the entry |
| Text inputs | `Ctrl`/`Alt` + `←` `→` move by word · `Ctrl+W` / `Alt+Backspace` delete word · `Ctrl+U` / `Ctrl+K` delete to the start or end of the line |

**Mouse:** click buttons and rows, scroll with the wheel, and drag to select text. The selection is copied to the clipboard.

---

## Troubleshooting

| Problem | Solution |
| --- | --- |
| `Current directory is not a git repository` | Run Wisetree inside a repository or one of its worktrees. |
| `wisetree requires a TTY for interactive mode` | You are piping or redirecting. Use `create`, `dashboard`, or `cache` with flags. |
| No PR data on the Dashboard | Set `dashboard.showPullRequests` to `true`, install `gh`, and run `gh auth login`. |
| `gh CLI not found - PR column hidden.` | `gh` is not on your `PATH`. |
| Wisetree does not change your directory | Install [shell integration](#shell-integration), reload your shell, and run `wisetree` with **no arguments**. |
| **Shared cache** is missing from the menu | It appears only when `worktreeLinkPatterns` is not empty. |
| `Branch 'x' already exists` | Pick another name, or leave the new-branch field blank to check out an existing branch. |
| The configuration file fails to parse | Fix the JSON at the path shown. If the broken file is the global config, you can press `R` on the error screen to reset it to the defaults. |

---

## Development

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets
cargo test --all-features
cargo run --bin generate-schema   # regenerate schema.json after changing the config schema
```

- CI runs on Ubuntu and macOS and treats warnings as errors.
- Tests of Git operations use real temporary Git repositories. TUI screens are covered by snapshot tests in `tests/`.

Project layout:

| Path | Contents |
| --- | --- |
| `src/cli/` | Argument parsing and non-interactive commands. |
| `src/tui/` | App, event loop, screens, and widgets. |
| `src/git/`, `src/worktree/`, `src/files/` | Git operations, worktree creation and deletion, copying, and the shared cache. |
| `src/services/dashboard.rs` | Dashboard polling and GitHub pull request operations. |
| `src/services/presets/` | Project setup presets and detection. |
| `src/services/shell_integration.rs` | Installation of the shell wrapper. |
| `src/config/` | Configuration schema, loading, and saving. |
| `tests/` | Integration tests and TUI snapshots. |

See [AGENTS.md](AGENTS.md) for contributor guidelines.

## License

MIT. See [LICENSE](LICENSE).

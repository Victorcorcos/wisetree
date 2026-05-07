# 🧙 Wisetree

<div align="center">
  <img src="https://i.imgur.com/vO0AOis.gif" alt="Wisetree" width="50%" />
</div>

**Wisetree** is an interactive, terminal-first manager for `git worktree`s. It wraps the raw `git worktree` plumbing in a polished TUI (built with Rust + Ratatui) and a scriptable CLI, so creating, listing, navigating, and deleting worktrees becomes a single keystroke instead of a paragraph of commands.

It is purpose-built for developers who run **multiple AI coding agents in parallel** — `Claude Code`, `Codex CLI`, `Gemini CLI`, `Opencode`, `Cursor`, `Aider`, and friends — each on its own branch, each in its own isolated checkout, all stacked on top of the same repository.

# 🤔 Why?

A `git worktree` lets a single repository have **multiple working directories checked out at the same time**, each pointing to a different branch. No stashing, no `git checkout` dance, no losing your place — just a clean, parallel copy of the codebase.

That capability is great. The raw ergonomics around it are not.

If you have ever tried to spin up three or four agents at once, you have probably hit some — or all — of the following:

- Re-typing long `git worktree add ../repo.feature-x main` invocations every single time.
- Forgetting to copy untracked but essential files like `.env`, `.env.local`, or `.vscode/settings.json` into the new worktree, and watching the agent fail because it cannot find a database URL or an API key.
- Re-running `bundle install`, `npm install`, `pnpm install`, `pip install`, `cargo build`, or `make setup` by hand every single time you spin up a new directory.
- Manually `cd`-ing into the new path, opening a new editor window, and only *then* starting the agent.
- Cleaning up dangling worktrees and stale branches one by one when the experiment is over.

`wisetree` collapses all of that into a single interactive session. It is opinionated where it helps you, and configurable where you need it.

The two features that make it especially powerful for AI-assisted development are:

| Feature | What it does | Why it matters for AI agents |
| --- | --- | --- |
| **Copy Patterns** (`worktreeCopyPatterns` / `worktreeCopyIgnores`) | Glob-based file copying from the source repo into the freshly created worktree, with an explicit ignore list. | Untracked-but-required files (`.env*`, `.vscode/**`, local credentials, editor settings) land in the new worktree automatically, so the agent can run, test, and inspect the code without any manual seeding step. |
| **Post-Create Commands** (`postCreateCmd`) | An ordered list of shell commands executed inside the new worktree right after it is created, with progress reporting in the TUI. | Bootstraps the environment — `bundle install`, `npm install`, `docker compose up -d`, `rails db:prepare`, `make seed`, anything — so by the time you hand control over to the agent, the project is already runnable. |

Combined with the optional **`terminalCommand`** (e.g. `code $WORKTREE_PATH`, `cursor $WORKTREE_PATH`, `idea $WORKTREE_PATH`) and the **shell integration** (which `cd`s your current shell into the selected worktree the moment you confirm), the loop becomes:

1. Run `wisetree`.
2. Pick a source branch and a name.
3. The new worktree is created, the relevant files are copied in, the setup commands run, your editor opens on it, and your shell is already inside it.
4. Launch your AI agent. Code.
5. Done? Run `wisetree` again, pick `Delete worktree`, and it's gone — branch and all, if you want.

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

### Shell integration (highly recommended)

Run `wisetree` once in any git repository and select **`Setup Shell Integration`** from the main menu. This installs a small wrapper into your `~/.zshrc` or `~/.bash_profile` / `~/.bashrc` that lets the bare command `wisetree` (with no args) `cd` your current shell into the selected worktree directly — no `cd $(...)` dance required. Tab-completion for subcommands and flags is installed at the same time.

# 🚀 Usage

Two complementary usage modes are supported: an **interactive TUI** for everyday human-driven flows, and a **non-interactive CLI** for scripts, automation, and CI.

### Interactive TUI

From inside any git repository, just run:

```rb
wisetree
```

You land on the main menu, where the available actions are:

| Menu entry | What it does |
| --- | --- |
| **Setup Shell Integration** | One-time installer for the shell wrapper + completions (only shown when integration is not yet installed). |
| **Create new worktree** | Guided flow: pick a source branch, name the directory, optionally name a new branch, confirm. Copy patterns, post-create commands, and terminal launch run automatically afterwards. |
| **List worktrees** | Live list of every worktree attached to the current repository, with the main checkout and any dirty trees called out. |
| **Delete worktree** | Pick a worktree, confirm, and it is gone. Optionally deletes the matching branch (`deleteBranchWithWorktree`). Falls back to manual cleanup for corrupted worktrees. |
| **Settings** | Inspect and edit the active configuration (project-local or global), reset to defaults, and toggle integration. |
| **Exit** | Close the TUI without changing anything. |

Each screen renders the **Monokai-inspired Wisetree palette** (defined in `design/pallete.md`) and supports `↑/↓` to navigate, `Enter` to confirm, and `Esc` / `Ctrl+C` to back out.

### Configuration

`wisetree` looks for configuration in this order, falling back to sensible defaults if neither is present:

1. `.wisetree.json` next to the repository root (project-local — commit it, share it across the team).
2. `~/.wisetree/settings.json` (global — your personal defaults).

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
  "worktreePathTemplate": "$BASE_PATH.worktree",
  "postCreateCmd": [
    "bundle install",
    "yarn install",
    "bin/rails db:prepare"
  ],
  "terminalCommand": "code $WORKTREE_PATH",
  "deleteBranchWithWorktree": true
}
```

The variables `$BASE_PATH`, `$WORKTREE_PATH`, `$BRANCH_NAME`, and `$SOURCE_BRANCH` are interpolated in `worktreePathTemplate`, `postCreateCmd`, and `terminalCommand` at runtime.

| Field | Type | Default | Purpose |
| --- | --- | --- | --- |
| `worktreeCopyPatterns` | `string[]` | `[".env*", ".vscode/**"]` | Glob patterns of files to copy from the source repo into a brand-new worktree. |
| `worktreeCopyIgnores` | `string[]` | `["**/node_modules/**", "**/dist/**", "**/.git/**", "**/Thumbs.db", "**/.DS_Store"]` | Glob patterns to skip during the copy step. |
| `worktreePathTemplate` | `string` | `"$BASE_PATH.worktree"` | Template that decides where new worktrees live on disk, relative to the repo's parent directory. |
| `postCreateCmd` | `string[]` | `[]` | Ordered list of shell commands executed inside the new worktree, with live progress in the TUI. |
| `terminalCommand` | `string` | `""` | Optional command spawned right after creation (e.g. `code $WORKTREE_PATH`) to open an editor or terminal. |
| `deleteBranchWithWorktree` | `boolean` | `false` | When `true`, deleting a worktree also deletes its associated branch. |

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
| `list` | List all worktrees attached to the current repository. |
| `delete` | Delete an existing worktree (interactive unless flags are supplied). |
| `settings` | Open the settings screen to inspect and edit configuration. |

### Global / interactive options

| Flag | Alias | Description |
| --- | --- | --- |
| `--help` | `-h` | Show the built-in help screen. |
| `--version` | `-v` | Print the installed version. |
| `--mode <mode>` | `-m` | Land directly on a specific screen (`menu`, `create`, `list`, `delete`, `settings`). |
| `--from-wrapper` | — | Used internally by the shell wrapper so `wisetree` can print the selected path on stdout for the parent shell to `cd` into. |

### Non-interactive options

| Flag | Alias | Applies to | Description |
| --- | --- | --- | --- |
| `--name <name>` | `-n` | `create`, `delete` | Worktree directory name. |
| `--source <branch>` | `-s` | `create` | Source branch to fork the new worktree from. |
| `--branch <branch>` | `-b` | `create` | New branch name; defaults to the directory name. |
| `--path <path>` | `-p` | `delete` | Worktree path (alternative to `--name`). |
| `--force` | `-f` | `delete` | Force-delete even when the worktree has uncommitted changes. |
| `--json` | — | `list` | Emit the worktree list as JSON, suitable for piping into `jq`. |

### Interactive examples

```rb
wisetree                # Open the main menu
wisetree create         # Jump straight into the create flow
wisetree list           # Browse worktrees interactively
wisetree delete         # Jump straight into the delete flow
wisetree settings       # Open the settings screen
```

### Non-interactive examples

```rb
wisetree create -n my-feature -s main                  # Create a worktree off main
wisetree create -n my-feature -s main -b feat/payments # Create with an explicit new branch
wisetree list --json                                   # Emit JSON for scripting
wisetree delete -n my-feature                          # Delete by directory name
wisetree delete -p /path/to/worktree -f                # Force-delete by full path
```

### Update checks

`wisetree` lazily checks the npm registry at most once every 24 hours and surfaces a banner inside the TUI when a newer version is available. The cache lives at `~/.wisetree/state.json` so the check never gets in your way.

# 🤝 Contribute

Contributions of every shape are welcome — bug reports, feature requests, design feedback on the palette, documentation polish, and code. The repository follows a few simple conventions:

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

# 🪪 License

`wisetree` is released under the **MIT License**. See [`LICENSE`](./LICENSE) for the full text. In short: do whatever you want with it, just keep the copyright notice intact.

# 👤 Creator

Victor Cordeiro Costa

1. Github: https://github.com/victorcorcos
2. Linkedin: https://www.linkedin.com/in/victorcorcos/

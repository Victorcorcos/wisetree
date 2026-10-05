# Wisetree

A terminal Git worktree manager built with Rust and Ratatui.

- **Create**: choose a source branch and create a worktree.
- **Dashboard**: list worktrees, inspect Git status, navigate, open your editor, and delete individual worktrees or groups.
- **Pull requests**: open an existing PR in your browser, close it, or squash-merge it using GitHub CLI. Merge asks for confirmation and warns about unpushed commits.
- **Settings**: configure worktree paths, copied files, optional shared dependency directories, post-create commands, and Dashboard preferences.

The main worktree is protected from deletion. Deleting another worktree asks for confirmation; optionally, its branch can also be deleted.

## Install

Requires Git and a stable Rust toolchain. GitHub CLI (`gh`) is optional and required only for pull request features; authenticate it with `gh auth login`.

```bash
git clone https://github.com/victorcorcos/wisetree.git
cd wisetree
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

## Use

Run inside a Git repository:

```bash
wisetree                  # interactive menu
wisetree create           # Create screen
wisetree dashboard        # Dashboard screen
wisetree settings         # Settings screen
wisetree create --name feature --source main --branch feat/feature
wisetree dashboard --json # non-interactive worktree list
wisetree --help
```

Use arrow keys to navigate, Enter to select, and Esc to return. Dashboard supports searching and bulk deletion. Enable pull request information in **Settings → Dashboard** to use Open, Close, and Merge.

The menu offers optional shell integration for navigating your current shell into a selected worktree. Shared cache management is available when using linked dependency directories.

## Configuration

Wisetree reads `.wisetree/config.json` in the project, then `~/.wisetree/config.json`, and uses built-in defaults when necessary. In a linked worktree, the main worktree's project configuration takes precedence. Keep `.wisetree/` out of Git.

Settings edits the active configuration. Dashboard preferences are `refreshIntervalMs` (5000–60000), `showPullRequests`, and `columns` (`branch`, `status`, `ahead_behind`, `diff`, `last_commit`, `pull_request`). Unknown legacy fields and unsupported column names are ignored when loading and omitted when saving.

The only other configuration keys are `worktreeCopyPatterns`, `worktreeCopyIgnores`, `worktreePathTemplate`, `postCreateCmd`, `worktreeLinkPatterns`, `worktreeLinkStrategy`, `worktreeLinkCacheDir`, `terminalCommand`, and `deleteBranchWithWorktree`. They control file copying, optional shared links, worktree paths, setup commands, editor launch, and branch deletion. See [schema.json](schema.json) for defaults and types. Commands can use `$BASE_PATH`, `$WORKTREE_PATH`, `$BRANCH_NAME`, and `$SOURCE_BRANCH`.

## Development

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets
cargo test --all-features
cargo run --bin generate-schema
```

CI checks Ubuntu and macOS. Tests use temporary Git repositories. See [AGENTS.md](AGENTS.md) for contributor instructions.

MIT licensed; see [LICENSE](LICENSE).

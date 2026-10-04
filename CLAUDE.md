# Wisetree

Rust + Ratatui manager for creating, listing, navigating, and deleting Git worktrees. Dashboard also opens, closes, and squash-merges existing GitHub pull requests through `gh`.

## Guidelines

- State assumptions when scope is unclear. Prefer the smallest working change.
- Change only what the task requires; avoid speculative abstractions.
- Run Git as a subprocess through the existing async helpers.
- Screens own their state and expose `handle_key` and `render`; App handles asynchronous work and routing.
- Keep user-facing strings in `src/messages.rs`.
- Keep configuration JSON keys in camelCase and regenerate `schema.json` after schema changes.
- Never commit `.wisetree/` or personal configuration. Project config is `.wisetree/config.json`; global config is `~/.wisetree/config.json`.
- Protect the main worktree from deletion and retain confirmations for destructive actions.
- Use real temporary Git repositories for tests of Git operations.

## Structure

- `src/cli/`: argument parsing and non-interactive commands.
- `src/tui/`: App, event loop, screens, and widgets.
- `src/git/`, `src/worktree/`, `src/files/`: Git operations and worktree creation/deletion.
- `src/services/dashboard.rs`: Git polling and GitHub pull request operations.
- `src/services/presets/`: project setup defaults.
- `src/config/`: configuration loading, saving, and defaults.
- `tests/`: integration tests and TUI snapshots.

## Required checks

There are no Git hooks. CI checks Ubuntu and macOS with warnings treated as errors. Before finishing changes, run and fix every failure:

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets
cargo test --all-features
```

Build the executable with `cargo build --release`. Regenerate the configuration schema with `cargo run --bin generate-schema`.

`AGENTS.md` and `GEMINI.md` link to this file; keep them consistent.

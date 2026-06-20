# Wisetree

Terminal-first Git worktree manager built in Rust + Ratatui. Lets developers create, survey, and delete worktrees with single keystrokes. Designed for running multiple AI coding agents (Claude Code, Opencode, Codex, Gemini) in parallel on isolated checkouts.

## Guidelines

1. **Think before coding.** State your assumptions out loud. If the request is ambiguous, ask. If a simpler approach exists, push back. Stop when you are confused, name what is unclear, do not just pick one interpretation and run.
2. **Simplicity first.** Write the minimum code that solves the problem. No speculative abstractions. No flexibility nobody asked for. The test: would a senior engineer call this overcomplicated.
3. **Surgical changes.** Touch only what the task requires. Do not improve neighboring code. Do not refactor what is not broken. Every changed line should trace back to the request.
4. **Goal-driven execution.** Turn vague instructions into verifiable targets before writing a line. "Add validation" becomes "write tests for invalid inputs, then make them pass."

Development: symlink the binary and rebuild to test changes without reinstalling.

```bash
ln -sf "$PWD/target/release/wisetree" /opt/homebrew/bin/wisetree
```

Git hooks in `githooks/` auto-run `cargo fmt`, `cargo clippy --fix`, and tests on push. Enable once with `git config core.hooksPath githooks`.

## Architecture

```
src/
├── main.rs / lib.rs           # entry point → cli::run()
├── cli/                       # argument parsing, routes to TUI or non-interactive commands
├── tui/
│   ├── app.rs                 # central App state machine; screen routing + async channels
│   ├── router.rs              # Screen enum (Menu, Create, Dashboard, Settings, …)
│   ├── event.rs               # crossterm input loop → AppEvent channel
│   ├── screens/               # ~15 screens; each owns state + handle_key() + render()
│   └── widgets/               # reusable UI components (SelectPrompt, PTY view, toast, …)
├── services/
│   ├── dashboard.rs           # live polling: git status, gh PR queries, AI status
│   ├── ai_status/             # detects Claude/Opencode/Codex/Gemini activity from disk files
│   ├── presets/               # 20+ project presets (Rails, Django, Next.js, Rust, …)
│   └── app_state.rs           # ~/.wisetree/state.json (update-check cache)
├── git/                       # thin async wrapper around git binary (no libgit2)
├── worktree/service.rs        # high-level create/delete orchestration (git + config + files)
├── files/                     # copy patterns, symlink cache, PTY post-create commands
├── config/                    # loads .wisetree.json (project) or ~/.wisetree/settings.json (global)
└── errors.rs                  # WisetreeError + GitErrorCode; user_friendly_message()
```

## Key Conventions

- **Git as subprocess**: all git ops spawn the `git` binary via `tokio::Command`; parse stdout/stderr.
- **Screen pattern**: each screen struct has `new()`, `handle_key()`, `render()`. Heavy state lives in the screen, not in widgets.
- **Async everywhere**: TUI runs on a Tokio multi-threaded runtime; background tasks send results back via `AppEvent` channels.
- **Config fallback**: project-local `.wisetree.json` → global `~/.wisetree/settings.json` → built-in defaults.
- **Serde field names**: use `camelCase` (`#[serde(rename_all = "camelCase")]`) to match the upstream TypeScript wire format.
- **Error handling**: propagate with `?`; map git stderr substrings to `GitErrorCode` variants for branching logic.
- **PTY rendering**: `portable-pty` spawns shells; `vt100` parses escape sequences for ratatui display.
- **AI status detection**: file-based; reads session/state files from each harness on disk (capped at 200 ms/tick).
- **Entry point**: bare `wisetree` opens the TUI on the Menu screen; `--mode <create|dashboard|cache|settings>` (or a positional like `wisetree dashboard`) lands on another screen. A subcommand runs **non-interactively** only when paired with flags/actions (`create --name …`, `cache prune`); otherwise it just opens that TUI screen. Parsing lives in `cli/args.rs`.

## Important Gotchas

- `.claude/` is in `.gitignore` — never place `CLAUDE.md` there; it will silently not be committed.
- `.wisetree.json` at the repo root is also gitignored (personal config override); the shared project config lives elsewhere.
- Dashboard PR fetching uses `gh pr list`; after a 429 it backs off for 5 minutes.
- Remotes: `origin` = victorcorcos/wisetree, `upstream` = same upstream project, `anderson` = fork. PRs go to `origin/main`.
- CI runs on Ubuntu and macOS; format + clippy + build + tests must all pass before merge.
- User-facing strings live in `src/messages.rs`; keep them in sync with the upstream TS catalog when changing behavior.

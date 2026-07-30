# Wisetree

Terminal-first Git worktree manager built in Rust + Ratatui. Lets developers create, survey, and delete worktrees with single keystrokes. Designed for running multiple AI coding agents (Claude Code, Opencode, Codex, Gemini) in parallel on isolated checkouts.

## Guidelines

1. **Think before coding.** State your assumptions out loud. If the request is ambiguous, ask. If a simpler approach exists, push back. Stop when you are confused, name what is unclear, do not just pick one interpretation and run.
2. **Simplicity first.** Write the minimum code that solves the problem. No speculative abstractions. No flexibility nobody asked for. The test: would a senior engineer call this overcomplicated.
3. **Surgical changes.** Touch only what the task requires. Do not improve neighboring code. Do not refactor what is not broken. Every changed line should trace back to the request.
4. **Goal-driven execution.** Turn vague instructions into verifiable targets before writing a line. "Add validation" becomes "write tests for invalid inputs, then make them pass."

## Build & Test

```bash
cargo build --release          # binary → target/release/wisetree
cargo test --all               # full suite (uses real git repos via tempdir, no mocks)
cargo fmt --all                # formatter
cargo clippy --all-targets -- -D warnings  # linter (CI enforces -D warnings)
```

Development: symlink the binary and rebuild to test changes without reinstalling.

```bash
ln -sf "$PWD/target/release/wisetree" /opt/homebrew/bin/wisetree
```

## CI Gates (MANDATORY — this repo has no git hooks)

There are **no `pre-commit` / `pre-push` hooks**: nothing runs automatically on commit or push.
CI is therefore the first thing that ever checks the code, and a red CI blocks the PR.
**You are responsible for producing code that already passes every gate below.**

CI (`.github/workflows/ci.yml`, on Ubuntu **and** macOS, with `RUSTFLAGS: -D warnings`) runs, in order:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets
cargo test --all-features
# reviewer benchmark deterministic gates
cargo run --quiet --bin reviewer_corpus -- check benchmarks/reviewer/corpus.public.json
cargo run --quiet --bin reviewer_benchmark -- benchmarks/reviewer/corpus.json benchmarks/reviewer/captured/pipeline.fixture.json benchmarks/reviewer/captured/skill.fixture.json
cargo run --quiet --bin reviewer_superiority -- check-status benchmarks/reviewer/superiority-status.json
```

### What this means when you write code

- **Formatting is not optional.** Run `cargo fmt --all` before you call a change done. Never hand-format; let rustfmt decide.
- **Zero warnings.** `-D warnings` covers both rustc warnings and clippy lints. Unused imports, unused variables, dead code, needless clones, `redundant_closure`, etc. all fail CI exactly like a compile error. Do not silence them with blanket `#[allow(...)]` — fix the cause; a targeted `#[allow]` needs a comment justifying it.
- **All targets, all features.** Tests, benches, examples and the extra `reviewer_*` binaries are compiled too. A warning that only appears in a `#[cfg(test)]` module still fails CI.
- **Tests must pass and must be real.** The suite drives actual git repos in tempdirs — no mocks. New behavior needs a test; changed behavior needs its test updated.
- **Benchmark fixtures are checked in.** If you touch the reviewer pipeline/skill, regenerate and commit the fixtures under `benchmarks/reviewer/` so the deterministic gates still pass.
- **Both OSes.** Avoid macOS-only or Linux-only assumptions (path separators, `/opt/homebrew`, GNU-vs-BSD CLI flags, case-sensitive filesystems).

### Before finishing a task, run

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Fix everything they report before reporting the work done. If you cannot run them, say so explicitly instead of implying they passed.

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

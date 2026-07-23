# Codex CLI and Claude Code as inner terminals in Wisetree

## Goal

Let a developer choose **Codex CLI** or **Claude Code** — instead of OpenCode —
as the embedded AI harness for each of Wisetree's twelve AI-assisted commands,
running as live inner terminals in the AI Activity panel exactly the way
OpenCode runs today: the real interactive TUI in the PTY, with Wisetree
detecting turn completion and extracting the assistant transcript to advance the
workflow.

This document is the implementation guide. It is meant to be split into sections
and handed to an AI, section by section, to plan and implement.

## Summary

This is viable. Codex CLI and Claude Code both start in Wisetree's existing PTY
with an initial prompt, an explicit model, and an explicit reasoning/thinking
level. The PTY layer is already generic enough to host either executable.

The real work is not PTY compatibility. It is generalizing the OpenCode-specific
turn lifecycle into a harness-neutral one:

- Today `Develop` (and the other interactive commands) start an OpenCode TUI
  that stays open after its turn.
- Wisetree detects completion and extracts the assistant transcript by reading
  OpenCode's private SQLite database (`OpencodeTurnWatcher`).
- The workflow parses that transcript, advances to the next state, and may kill
  the still-open TUI.

Codex and Claude expose the same lifecycle on disk, and Wisetree already parses
both formats for the dashboard. The plan below reuses that parsing to build a
`CodexTurnWatcher` and a `ClaudeTurnWatcher` modeled on `OpencodeTurnWatcher`,
so the interactive inner terminal works for every harness.

## Execution strategy per slot

Two run modes are used, chosen by what the slot already is:

- **Interactive (primary).** Render the real `codex` / `claude` TUI in the PTY
  and auto-advance with a harness-specific turn watcher. Used for every slot
  that is interactive under OpenCode today: `explain`, `fix.apply`, `update`,
  `bugkill.investigate`, `bugkill.fix`, `develop.plan`, `develop.implement`.
- **One-shot captured.** Run `codex exec` / `claude -p`, take completion from
  process exit and the transcript from documented output. Used only for slots
  that are already non-interactive captured runs under OpenCode: `fix.plan`,
  `review.strong/balanced/utility`, `bugkill.judge`.

This keeps the inner-terminal experience for interactive commands and uses the
cleaner one-shot contract only where there was never an interactive UI to begin
with.

## What Wisetree does today

- `src/tui/widgets/pty_view.rs:60` is a generic `portable-pty` host. It accepts
  an executable, argument vector, cwd, and environment; sets
  `TERM=xterm-256color`; renders with `vt100`; forwards input; and reports the
  child exit code. Nothing in the spawn mechanism requires OpenCode.
- `src/services/dashboard.rs:4753` and `:4808` build the Develop planning and
  implementation launches. Both pass a generated prompt and model to the
  interactive OpenCode TUI. Planning also selects OpenCode's read-only `plan`
  agent.
- `src/services/dashboard.rs:4789` and `:4840` seed OpenCode's private
  `model.json` because its TUI does not accept a thinking flag.
- `src/services/opencode_turn.rs:1` polls `opencode.db` for completion and
  reconstructs the clean assistant transcript, because terminal capture of a TUI
  is escape-sequence soup and not suitable for contract parsing. It binds to one
  worktree at spawn time, pins `since_ms` so a retry never latches onto the
  previous run's session, and pins the resolved `session_id` so the user's own
  unrelated session in the same cwd cannot hijack the watch
  (`opencode_turn.rs:53`). It returns `Working | Finished { transcript } |
  Failed { message }` (`opencode_turn.rs:40`).
- `src/config/schema.rs:114` stores only `{ model, thinking }`, and its model
  value is specifically an OpenCode `provider/model` identifier. The struct has
  `#[serde(deny_unknown_fields)]`.

The existing dashboard activity detectors already parse the Codex and Claude
turn lifecycles and are the foundation for the new watchers:

- `src/services/ai_status/codex.rs:314` reads
  `~/.codex/sessions/.../rollout-*.jsonl` and interprets the turn-lifecycle
  events codex's `RolloutRecorder` writes verbatim — `task_started` /
  `turn_started` (in flight) vs. `task_complete` / `turn_complete` /
  `turn_aborted` (done). The last such marker in the file is the state.
- `src/services/ai_status/claude.rs:1` reads
  `~/.claude/projects/<slug>/*.jsonl` and keys on `message.stop_reason` — a
  `user` prompt or assistant `stop_reason == tool_use` is in flight; any other
  stop reason (`end_turn`, `stop_sequence`, `max_tokens`, …) is done.

These detectors currently reduce each cwd to a coarse `Running`/`Idle` for the
dashboard and do not return a transcript. Section 4 explains what to add so the
same parsing drives an embedded turn watcher.

## CLI capability comparison

| Capability | OpenCode today | Codex CLI | Claude Code |
| --- | --- | --- | --- |
| Start interactive UI with prompt | `--prompt <text>` | Positional `PROMPT` | Positional `prompt` |
| Select model | `-m provider/model` | `-m` / `--model` | `--model` |
| Select reasoning | TUI state-file seed | `-c 'model_reasoning_effort="high"'` | `--effort high` |
| Read-only planning | `--agent plan` | `--sandbox read-only` | Interactive: `--permission-mode plan`; one-shot: restrict `--tools` to read tools |
| Workspace implementation | OpenCode permissions | `--sandbox workspace-write` | Permission mode plus tool allow/deny rules |
| One turn, then exit | `opencode run` | `codex exec` | `claude -p` / `--print` |
| Structured live output | CLI-specific | `codex exec --json` (JSONL) | `--output-format stream-json` |
| Stable final-output capture | `opencode run` stdout or private DB | `-o` / `--output-last-message` | Final `result` in JSON/stream-JSON output |

Official references:

- [Codex CLI reference](https://developers.openai.com/codex/cli/reference)
  documents the positional prompt, `--model`, `--config`, sandbox, approval,
  `--no-alt-screen`, `codex exec --json`, and
  `--output-last-message` options.
- [Codex non-interactive mode](https://developers.openai.com/codex/noninteractive)
  documents that `codex exec` streams progress, exits after the run, supports
  JSONL lifecycle events, and defaults to a read-only sandbox.
- [Codex configuration reference](https://developers.openai.com/codex/config-reference)
  documents `model_reasoning_effort` and notes that supported effort values
  are model-dependent.
- [Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference)
  documents positional prompts, `--model`, `--effort`, permission modes,
  `--print`, and text/JSON/stream-JSON output.
- [Claude Code programmatic mode](https://code.claude.com/docs/en/headless)
  documents the final `result` field, real-time stream events, and the
  interaction between `--allowedTools` and non-interactive permission modes.
- [Claude Code permissions](https://code.claude.com/docs/en/permissions)
  documents the behavior of `plan`, `acceptEdits`, and `dontAsk`.
- [Claude Code hooks](https://code.claude.com/docs/en/hooks) documents that
  `AskUserQuestion` and `ExitPlanMode` normally block in non-interactive
  `-p` mode unless the caller supplies an interaction hook.

## Candidate invocations

These are argument-vector examples, not shell strings. Pass prompt text as one
argument so backticks, quotes, `$()`, and newlines cannot be interpreted by a
shell.

### Codex interactive (primary for interactive slots)

Planning:

```text
codex
  --model <codex-model>
  --config model_reasoning_effort="high"
  --sandbox read-only
  --ask-for-approval never
  <prompt>
```

Implementation:

```text
codex
  --model <codex-model>
  --config model_reasoning_effort="high"
  --sandbox workspace-write
  --ask-for-approval on-request
  <prompt>
```

Renders inside the current PTY. `--no-alt-screen` is available if inline
rendering behaves better inside the nested terminal. The TUI stays open after a
completed turn, so Wisetree advances via `CodexTurnWatcher` (section 4).

### Codex one-shot (for already-captured slots)

```text
codex exec
  --model <codex-model>
  --config model_reasoning_effort="high"
  --config approval_policy="never"
  --sandbox <read-only-or-workspace-write>
  --json
  --output-last-message <wisetree-owned-temp-file>
  <prompt>
```

JSONL provides activity events, the output file provides the exact final
assistant message, and the exit status provides success/failure. Planning uses
`read-only`; implementation needs `workspace-write`.

Flag placement matters. In Codex CLI 0.145.0,
`codex exec --ask-for-approval never` is rejected because the flag is global and
is not accepted in that post-subcommand position. Use
`--config approval_policy="never"`, which `exec` accepts directly and which maps
to the documented non-interactive configuration. It does not grant extra access:
the sandbox still limits the run, and operations that need unavailable approval
fail. Do not use `--dangerously-bypass-approvals-and-sandbox`.

`--output-last-message` and any `--json` capture must target a path unique per
concurrent run — keyed by run/worktree id, never a fixed path or "newest file in
cwd" — because Wisetree runs many worktree agents at once.

### Claude Code interactive (primary for interactive slots)

Planning:

```text
claude
  --model <claude-model-or-alias>
  --effort high
  --permission-mode plan
  <prompt>
```

Implementation:

```text
claude
  --model <claude-model-or-alias>
  --effort high
  --permission-mode default
  <prompt>
```

Renders in the current PTY and lets the user answer permission prompts while the
inner terminal is focused. Wisetree advances via `ClaudeTurnWatcher`
(section 4).

### Claude Code one-shot planning (for already-captured slots)

```text
claude
  --print
  --model <claude-model-or-alias>
  --effort high
  --output-format stream-json
  --verbose
  --include-partial-messages
  --permission-mode dontAsk
  --tools Read,Glob,Grep
  --disallowedTools mcp__*
  <prompt>
```

Do not use `--permission-mode plan` for one-shot planning. Although it is
read-only, Claude can call `ExitPlanMode` when the plan is ready or
`AskUserQuestion` while preparing it, and both normally block under `-p` unless
the caller implements an interaction hook. `dontAsk` plus an explicit read-only
`--tools` list is a smaller, reliably non-interactive contract. The generated
prompt should tell Claude to return the plan as its final response and not
request interaction.

If planning must use read-only shell commands such as `git diff`, add `Bash` to
`--tools` only together with narrowly scoped `--allowedTools` rules. Do not allow
arbitrary Bash merely to reproduce interactive plan mode.
`--disallowedTools mcp__*` is necessary because `--tools` restricts built-in
tools but does not restrict MCP tools.

### Claude Code one-shot implementation (for already-captured slots)

```text
claude
  --print
  --model <claude-model-or-alias>
  --effort high
  --output-format stream-json
  --verbose
  --include-partial-messages
  --permission-mode acceptEdits
  --tools Read,Glob,Grep,Edit,Write,Bash
  --allowedTools Bash(<exact-check-command>)
  --disallowedTools mcp__*
  <prompt>
```

`Bash(<exact-check-command>)` is one argument containing a Claude permission
rule, derived from the configured check command. Add other rules only for
commands Wisetree intentionally authorizes. `acceptEdits` allows file edits
without prompting; unapproved commands can still be denied instead of silently
broadening access. Never substitute `--dangerously-skip-permissions`.

For both one-shot forms, the stream carries activity and ends with a `result`
message, and the process exits after the turn. Claude does not document a
final-message file option, so Wisetree must capture and parse the stream itself.
Streamed final output is reliable only on Claude Code 2.1.214+: before 2.1.208 a
large piped response could omit the final `result`, and before 2.1.214 the
short output-drain wait could truncate the end of a large response. Treat
**Claude Code 2.1.214+ as a hard prerequisite** for the streamed one-shot
adapter and gate it in preflight; the alternative during a spike is
`--output-format json` without live token streaming.

## Required design changes

### 1. Add harness to the persisted per-command configuration

Keep one canonical `provider/model` value in the configuration and add a typed
harness:

```json
{
  "model": "openai/gpt-5.6-sol",
  "thinking": "high",
  "harness": "codex"
}
```

Use a dedicated execution enum rather than an unchecked string:

```text
AiCommandHarness = Opencode | Codex | ClaudeCode
```

The existing `services::ai_status::AiHarness` is related but not identical: it
also represents Gemini and uses the status-report wire values `codex_cli` and
`claude_code`. A separate three-value execution enum keeps the persisted command
contract narrow. JSON values are `opencode`, `codex`, and `claudeCode`; the UI
label for the last value is `claude code`.

`AiModelConfig` carries `#[serde(deny_unknown_fields)]`. Add
`#[serde(default)] harness` (a known field, so this stays compatible) and make
`AiCommandHarness::default()` return `Opencode`. That gives all of these cases
the required behavior:

- existing nested `{ model, thinking }` values become OpenCode;
- the older flat AI configuration migrated by `AiConfig::deserialize` becomes
  OpenCode for all twelve slots;
- absent per-command slots use their existing model/thinking defaults plus
  OpenCode;
- newly saved values serialize the harness explicitly.

Serialization tests must assert that pre-existing configs with no `harness` key
still deserialize, and that no other unknown key slips in.

The model stays canonical in storage because it is also what the existing model
picker returns. Translate only at the launcher boundary:

| Harness | Accepted stored provider | CLI model argument |
| --- | --- | --- |
| OpenCode | Any provider | Pass the complete `provider/model` value |
| Codex | Exactly `openai` | Strip `openai/` and pass the model slug |
| Claude Code | Exactly `anthropic` | Strip `anthropic/` and pass the model ID |

Do not infer provider from the model's marketing name. For example,
`github-copilot/gpt-*` is not an `openai` provider under this rule and therefore
offers only OpenCode.

The valid harness list for a row is consequently:

```text
openai/*    -> [opencode, codex]
anthropic/* -> [opencode, claudeCode]
everything  -> [opencode]
```

If the model is changed and the current harness becomes incompatible, reset the
harness to OpenCode and mark the row modified. A hand-edited configuration that
explicitly combines an incompatible provider and harness must fail validation
with the slot path and accepted choices; it must not silently run a different
executable.

### 2. Implement the AI Models interaction

The screen is already `Settings -> Dashboard -> ai`, backed by
`AiSettingsEditor`. Change the description to:

```text
Pick a model + thinking strength + harness per AI command:
```

Render each non-empty row as three independently styled spans:

```text
openai/gpt-5.6-sol  ·  medium  ·  opencode
```

Add a field-focus value to the editor:

```text
AiSettingsField = Model | Thinking | Harness
```

`AiSettingsSelection` continues to own vertical location (`Rect(index)`,
`FreeModels(index)`, or `Save`); `AiSettingsField` owns the horizontal focus only
while a rectangle is selected. This is less invasive than multiplying every
selection variant by three and preserves the existing scroll window, free-model
row, and Save navigation.

Keyboard contract:

| Location | Key | Behavior |
| --- | --- | --- |
| Command row | Up/Down or `j`/`k` | Move between command rows, preserving the focused column |
| Command row | Left/Right or `h`/`l` | Move focus between Model, Thinking, and Harness; clamp at the ends |
| Model focused | Enter | Open the model picker |
| Thinking focused | Space | Advance through Default plus the supported levels, circularly |
| Harness focused | Space | Advance through the compatible harness list, circularly |
| Free-model row | Left/Right, Enter | Keep the existing chip-cycle and stage behavior |
| Save | Enter | Persist the complete Dashboard configuration |

Space on Model and Enter on Thinking/Harness are inert. This keeps one obvious
key per action. Footer:

```text
Up/Down move · Left/Right choose field · Space change value · Enter pick model/Save · Esc back
```

Today `render_ai_settings_rectangle` applies bold white styling to the whole
line and dims only the thinking suffix. Change it to style the focused span
white + bold and the other two spans muted/dim, leaving separators muted.
Default focus when the page opens is Model. The rectangle border and
Saved/Modified colors are unchanged.

**Model-picker source.** The picker is fed by `opencode models` today. Define
the model source per harness so a user selecting Codex/Claude picks a valid
model: use `codex debug models --bundled` for Codex, and a curated or
`--help`-derived list for Claude. If instead the picker keeps showing OpenCode's
catalogue and only the harness column re-interprets an already-`openai/`- or
`anthropic/`-prefixed model, state that explicitly and enforce the provider
rules from section 1.

The first-page AI summary used by Pull Request commands must also expose the
choice. Extend the shared `PrConfirmView` table from:

```text
Role | Model | Thinking
```

to:

```text
Role | Model | Thinking | Harness
```

Harness is the last column and uses the display labels `Opencode`, `Codex`, or
`Claude Code`. This affects every current caller of `AiRoleRow`: Explain, Fix,
Review, Update, Bugkill, and Develop. Add `harness` to `AiRoleRow` and pass it
from each role's `AiModelConfig`; do not re-read a settings file in the widget.
The screen already receives the resolved Dashboard configuration, which comes
from project-local `.wisetree.json` when present and otherwise from the global
settings file. Legacy entries have `Opencode` through the serde default, so that
is what the new column displays.

Widen the centered table enough for the new fixed-width column and let Model
remain the flexible/clipped column on narrow terminals. The table's height is
unchanged, but its render tests and all `AiRoleRow::new` call sites must be
updated.

The model picker also asks for a thinking variant after model selection. With
inline Space handling, that second picker phase becomes duplicative and, for
Codex/Claude, can use the wrong harness's variants. Simplify
`AiModelPickerAction::Selected` to return the model only. On return:

- preserve the current thinking value if it is supported by the current
  harness/model pair;
- otherwise reset thinking to Default;
- reset an incompatible harness to OpenCode as described in section 1.

The free OpenCode model chips need the same normalization, because selecting an
`opencode/*` chip while Codex or Claude is selected makes that harness invalid.

Mouse support currently exists for the AI Save button but not individual
command-row fields. Keyboard behavior is sufficient; if row mouse targets are
added later, each span needs its own hit rectangle so a click selects the correct
field rather than opening the model picker unconditionally.

### 3. Make thinking choices harness-aware

The current variant map is keyed only by `provider/model` and comes from
`opencode models --verbose`. It cannot be the source of truth for other
harnesses. Resolve choices by `(harness, provider/model)`:

- OpenCode: keep the existing per-model variant map and generic fallback.
- Codex: parse `codex debug models --bundled`, whose JSON includes each model
  slug's `supported_reasoning_levels`. The installed 0.145.0 binary exposes this
  command without making a paid model call.
- Claude Code: the CLI documents `--effort` and prints the levels supported by
  that CLI version, but does not document an equivalent per-model catalogue. Use
  the locally advertised levels as a best-effort list, retain Default, and treat
  model-specific acceptance as a launch preflight. Do not hardcode an
  OpenCode-derived ladder as authoritative for Claude.

The ladders genuinely differ: the generic Codex config reference lists
`minimal|low|medium|high|xhigh`, while the installed Codex catalogue offers
`low|medium|high|xhigh|max|ultra` for `gpt-5.6-sol`; installed Claude Code
2.1.198 advertises `low|medium|high|xhigh|max`, and newer Claude versions add
`ultracode` on supported models. One shared ladder would accept invalid
combinations or hide valid ones.

When Space changes the harness, immediately re-resolve the thinking list. Keep
the current value if it remains valid; otherwise reset it to Default in the same
row mutation. This prevents saving a thinking level that was valid for OpenCode
but invalid for Codex or Claude.

### 4. Provider-neutral run contract and turn watcher

Generalize the workflow boundary. Replace
`FixApplyHandoff { opencode_binary, opencode_args, cwd }` and the direct
`OpencodeTurnWatcher` references with:

```text
AiRunSpec {
    harness,
    binary,
    args,
    cwd,
    output_mode,   // Interactive | OneShot { capture }
}

AiRunEvent = Activity | Finished { transcript } | Failed { message }
```

Define a shared turn watcher with one implementation per harness, all returning
the same enum `OpencodeTurn` already uses
(`Working | Finished { transcript } | Failed { message }`):

```text
trait AiTurnWatcher {
    fn poll(&mut self) -> AiTurn;   // Working | Finished { transcript } | Failed { message }
}

// implementations:
OpencodeTurnWatcher   // existing: opencode.db
CodexTurnWatcher      // new: ~/.codex/sessions/.../rollout-*.jsonl
ClaudeTurnWatcher     // new: ~/.claude/projects/<slug>/*.jsonl
```

The new watchers reuse the lifecycle parsing already written and tested in
`ai_status/codex.rs` and `ai_status/claude.rs`, plus two things those detectors
do not do today:

1. **Attribution, not just detection.** The detectors collapse each cwd to a
   coarse `Running`/`Idle`, keeping only the newest-mtime session. A watcher
   must mirror `OpencodeTurnWatcher`'s anti-hijack discipline: pin `since_ms` at
   spawn so a retry never latches onto the previous run's session, and pin the
   resolved session id/file so the user's own separate `codex`/`claude` session
   in the same cwd cannot hijack the watch. Worktree cwds are unique, which
   makes cwd a strong key, but "newest session in cwd" alone is the *Same-cwd
   session races* risk below.
2. **Transcript extraction.** The detectors return only `Running`/`Idle`;
   workflows that parse a contract (`fix.plan`, `explain`, `bugkill.judge`, …)
   need the assistant text. Extract it from the same files the detectors already
   locate: for Codex the assistant `response_item` / `final_answer` message
   parts, for Claude the assistant `message` content blocks. This is the
   equivalent of `OpencodeTurn::Finished { transcript }`.

Keep the existing manual "continue now" Enter fallback (`bugkill_pr.rs:846`) as
the guaranteed backstop if a session file is missing or a CLI changes format.

For the one-shot slots, the same `AiTurn` values come from process exit plus
documented output (`--output-last-message` for Codex, the stream `result` for
Claude) instead of a file watcher.

### 5. Route every configured AI phase through the selected harness

The harness is stored on all twelve `AiModelConfig` leaves, not only Develop.
The current execution paths are:

| Settings slot | Current OpenCode behavior | New run mode |
| --- | --- | --- |
| `explain` | Interactive TUI + turn watcher | Interactive + turn watcher |
| `fix.plan` | Captured `opencode run`, parse structured text | One-shot captured |
| `fix.apply` | Interactive TUI + turn watcher | Interactive + turn watcher |
| `review.strong/balanced/utility` | Multiple captured `opencode run` calls | One-shot captured |
| `update` | Interactive TUI + turn watcher (PR and local-branch conflicts) | Interactive + turn watcher |
| `bugkill.investigate` | Interactive TUI + turn watcher | Interactive + turn watcher |
| `bugkill.fix` | Interactive TUI + turn watcher | Interactive + turn watcher |
| `bugkill.judge` | Captured `opencode run` | One-shot captured |
| `develop.plan` | Interactive TUI + turn watcher | Interactive + turn watcher |
| `develop.implement` | Interactive TUI + turn watcher | Interactive + turn watcher |

Every call site must request an `AiRunSpec` from a harness adapter rather than
reading a global `opencode_binary`. In particular:

- binary availability checks must inspect the selected slot's executable;
- `FixApplyHandoff`, `ConflictsHandedOffToUi`, and similar outcomes must carry a
  provider-neutral run specification;
- the five `OpencodeTurnWatcher` fields in `App` must become `AiTurnWatcher`
  trait objects (or a harness-tagged enum) so the same App state serves any
  harness;
- screen methods and labels such as `spawn_opencode_pty`, `Focus opencode`, and
  `Launching opencode...` must use the selected harness display name;
- `PrConfirmView` must add Harness as the last column after Role, Model, and
  Thinking, populated from the already-resolved per-role configuration;
- Review benchmark/capture provenance should include harness so results from
  different executables cannot be compared as if they were identical.

Rollout rule: do not display Codex or Claude for a slot until that slot's
complete execution path supports the adapter. If the first implementation covers
only `develop.plan` and `develop.implement`, the harness list for the other ten
slots must temporarily remain `[opencode]`.

The executable names are `opencode`, `codex`, and `claude`. Keep the product
labels `OpenCode`, `Codex CLI`, and `Claude Code` separate from binary names. At
launch, convert the stored canonical model as described above, build the
harness-specific arguments, and return an actionable error naming the missing
binary, unsupported model, or unsupported effort.

### 6. Preserve the existing local/global save semantics

No new persistence path is needed. For an AI-only edit, the existing flow already
does what is required:

1. AI Settings Save returns `SettingsAction::SaveDashboard`.
2. `App::save_dashboard` writes the whole dashboard to the existing
   project-local `.wisetree.json` when that file exists.
3. If no local file exists, it writes `~/.wisetree/settings.json`.
4. It reloads the active `ConfigService` and marks the Dashboard editor saved.

Esc currently stages AI edits back into the Dashboard editor but does not write
them. Preserve that behavior; only Enter on the AI page's Save button persists.
Add tests for both target paths with a non-default harness, as the existing tests
already cover the local/global routing for Dashboard settings.

One existing exception deserves a regression test: `App::save_dashboard` forces a
project-local write when `wiseMerge` changed, even if no local file previously
existed. If a user stages `wiseMerge`, enters AI Settings, and saves there, that
special case currently wins over the global fallback. Either keep that
established Dashboard behavior and document it, or ensure AI Settings Save cannot
accidentally include an unrelated staged `wiseMerge` change. The harness field
itself must not create a local file when none exists.

### 7. Capture output independently of terminal rendering

`PtyView` currently sends bytes only to the `vt100` parser. For one-shot slots,
extend the reader to tee raw bytes/events to a bounded capture or channel.

- Codex renders its normal progress in the panel and Wisetree reads the final
  message from `--output-last-message` (or parses `--json`).
- Claude stream-JSON is parsed into a small provider-neutral activity view rather
  than displayed as raw JSON lines.
- Keep a strict memory cap and retain only the final transcript plus a bounded
  activity tail.
- Capture files/temp paths must be unique per concurrent run (section on Codex
  one-shot above).

Interactive slots do not rely on this: they render the real TUI and take the
transcript from the turn watcher (section 4), exactly like OpenCode today.

### 8. Separate planning and implementation permissions

The OpenCode `--agent plan` is semantically important, not just a UI choice. Map
it per harness and run mode:

- Codex planning: `--sandbox read-only`.
- Codex implementation: `--sandbox workspace-write`.
- Claude interactive planning: `--permission-mode plan`.
- Claude one-shot planning: `--permission-mode dontAsk` with a strict read-only
  `--tools` list; do not expose `ExitPlanMode` or `AskUserQuestion` unless
  Wisetree implements their interaction hook.
- Claude implementation: `--permission-mode acceptEdits` with an explicit tool
  list and narrowly scoped `--allowedTools` rules for configured checks.

Show the effective permission policy on the confirmation page alongside harness,
model, and thinking level.

### 9. Capability and authentication preflight

`<binary> --version` alone is not enough. Versions and entitlements differ, and
effort levels are model-dependent. Preflight must report:

- binary absent;
- not authenticated;
- requested model unavailable;
- requested effort unsupported;
- permission/output flag unsupported by the installed CLI version.

Authentication and billing diverge by harness and this is a first-class concern
for a tool that runs agents in parallel across many worktrees. The three CLIs
authenticate differently (OpenCode config keys, Codex login, Claude
subscription/OAuth), and a user may be logged into one but not another, or incur
per-invocation metering that parallel fan-out multiplies. Surface an actionable
"authenticate `<harness>` first" state distinct from "binary absent," and
document the auth model per harness.

Local baseline for development: Codex CLI 0.145.0 and Claude Code 2.1.198 are
installed. Their `--help` output confirms the prompt/model/effort and
structured-output flags above, and local parsing confirmed that Codex rejects
`--ask-for-approval` after `exec` and accepts `--config approval_policy="never"`.
Claude 2.1.198 is adequate for argument-construction tests but must be upgraded
to 2.1.214+ before relying on streamed final output (see the Claude one-shot
section).

### 10. Code areas and verification

The implementation is wider than the Settings row itself:

- `src/config/schema.rs`: add `AiCommandHarness`, the defaulted field, legacy
  migration, validation, defaults, JSON Schema output, and serialization tests.
  Every `AiModelConfig` struct literal in source/tests must either set harness or
  use a constructor/default update.
- `src/tui/screens/settings.rs`: add horizontal field focus, Space cycling,
  provider filtering, harness-aware thinking lookup, per-span styling,
  normalization, help text, and local editor tests.
- `src/tui/screens/ai_model_picker.rs` and `src/services/opencode_models.rs`:
  make model selection independent of the OpenCode-only variant phase, add the
  per-harness model source, and introduce harness-aware capability sources.
- `src/services/dashboard.rs`: replace hard-coded OpenCode binary gates, argument
  construction, output parsing, handoff types, and messages with adapters.
  Preserve the existing prompt builders and deterministic parsers.
- `src/services/opencode_turn.rs` (or a new `ai_turn` module): extract the shared
  `AiTurnWatcher` trait and add `CodexTurnWatcher` / `ClaudeTurnWatcher` reusing
  `ai_status/{codex,claude}.rs` parsing, with attribution pinning and transcript
  extraction.
- `src/tui/app.rs`: resolve the correct binary, own structured child output for
  one-shot slots, and generalize the watcher fields to the trait.
- `src/tui/screens/{explain_pr,fix_pr,update_pr,review_pr,bugkill_pr,develop_pr}.rs`
  and `src/tui/widgets/pr_confirm.rs`: make activity labels and confirmation
  tables harness-aware without changing the workflow state machines.
- `src/messages.rs`, README configuration examples, and any generated schema:
  document the new field and keep user-facing catalog strings synchronized.

Minimum verification matrix:

- old nested and old flat JSON both deserialize to OpenCode;
- each compatible provider/harness pair round-trips through JSON;
- incompatible hand-edited pairs produce an actionable validation error;
- Left/Right changes focus without changing a value;
- Space cycles thinking and harness values circularly and marks only that row;
- changing model/harness resets an invalid thinking value to Default;
- selecting a free OpenCode model resets an incompatible harness;
- Save writes harness to local config when present and global config otherwise;
- each adapter emits the exact argument vector for prompt, model, effort, cwd,
  permissions, and output mode without invoking a shell;
- `CodexTurnWatcher` / `ClaudeTurnWatcher` classify a fixture session as
  Working, then Finished with the right transcript, and ignore a same-cwd
  session created before `since_ms` or with a different session id;
- stub Codex/Claude one-shot processes exercise structured progress, final
  transcript, non-zero exit, malformed output, timeout, and cancellation;
- confirmation views show the selected harness for every role.

`dashboard.aiStatus.enabledHarnesses` is a separate feature: it controls
background detection of arbitrary external AI sessions. Selecting Codex or Claude
for a command must not silently rewrite that monitoring preference, and the
embedded AI Activity panel must track its own child regardless of the background
detector setting.

## Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Reading private session files | CLI updates can break completion or transcript parsing | Reuse the existing dashboard parsing (already maintained for Codex/Claude); keep the manual "continue now" fallback; version-check in preflight |
| Same-cwd session races | A watcher can attach to a user's unrelated session | Pin `since_ms` and the resolved session id/file, not "newest session in cwd" |
| Model ID mismatch | Existing OpenCode values are invalid in Codex/Claude | Add `harness` and validate models per harness |
| Effort mismatch | Levels vary by model and CLI version | Discover capabilities per harness and allow "default" |
| Harness exposed before its slot is implemented | Settings saves Codex/Claude but runtime still launches or watches OpenCode | Gate each slot's harness choices on completed adapter coverage |
| Model change leaves stale harness/effort | Saved combination is invalid at launch | Normalize both harness and thinking immediately after model or harness changes |
| Unattended permission prompt | Run stalls or fails midway | Use explicit permission policies; avoid Claude `plan` under `-p` unless an interaction hook handles `AskUserQuestion` and `ExitPlanMode` |
| Truncated Claude stream | Older Claude versions can omit part or all of the final `result` | Require Claude Code 2.1.214+ for stream-JSON, or use single-result JSON during a spike |
| Raw JSON in the PTY | Poor AI Activity UX | Parse structured streams into provider-neutral activity rows |
| Nested TUI differences | Mouse, alternate screen, resize, or key handling may differ | Spike interactive rendering on macOS/Linux; test Codex with and without `--no-alt-screen` |
| Authentication/subscription differences | Binary exists but cannot run the selected model | Add an auth/model preflight and actionable error text |
| Parallel-run output collisions | Concurrent one-shot runs clobber each other's capture files | Key temp files by run/worktree id |
| Instruction-file differences | Harnesses load different project/user instructions (`AGENTS.md`, `CLAUDE.md`, OpenCode config) | Keep workflow-critical constraints in the generated prompt and document harness-specific instruction loading |

## Recommended rollout

1. Add the defaulted harness schema, provider validation, model translation, and
   Settings sub-focus/Space behavior. Keep every slot's visible harness list at
   `[opencode]` until its runtime adapter is ready.
2. Introduce `AiRunSpec`, the `AiTurnWatcher` trait, and `CodexTurnWatcher`.
   Implement interactive `develop.plan` and `develop.implement` with Codex.
   Enable Codex only for those two OpenAI-backed slots.
3. Add `ClaudeTurnWatcher` and interactive Claude planning/implementation
   permission policies. Enable Claude for those two slots when the model provider
   is Anthropic. Require Claude Code 2.1.214+ where streamed one-shot output is
   used.
4. Extend the adapters to the one-shot captured phases (`fix.plan`, Review,
   `bugkill.judge`), then the remaining interactive slots (Explain, Fix apply,
   Update, Bugkill).
5. Add Harness to confirmation tables and provenance, convert remaining
   OpenCode-specific UI strings, and enable all compatible choices.

## Final assessment

- **Can Codex CLI run as an inner terminal in the PTY?** Yes.
- **Can Claude Code run as an inner terminal in the PTY?** Yes.
- **Can both receive prompt, model, and thinking/effort at launch?** Yes.
- **Is this a command-line substitution only?** No. The completion and transcript
  lifecycle is currently OpenCode-specific and must be generalized into
  `AiTurnWatcher` with a Codex and a Claude implementation.
- **Does the completion-detection logic already exist?** Yes. `ai_status/codex.rs`
  and `ai_status/claude.rs` already parse both turn lifecycles for the dashboard;
  the watchers reuse that parsing plus attribution pinning and transcript
  extraction.
- **Will the Settings interaction work?** Yes. It fits the existing
  `AiSettingsEditor` cleanly and the existing Save path already has the desired
  local/global behavior.
- **Is it practical to implement?** Yes. Develop-only support is moderate;
  supporting the harness selector across all twelve slots is a larger
  cross-cutting change because the current runtime and UI lifecycle are
  OpenCode-specific.
- **Best first target:** interactive Codex on the Develop page via
  `CodexTurnWatcher`, then interactive Claude once its watcher and permission
  policies are in place.

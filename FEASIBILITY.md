# Codex CLI and Claude Code in Wisetree's AI Activity panel

## Conclusion

Yes, this is viable to implement Codex CLU and Claude Code inner terminals (AI Activity) inside Wisetree, just like Opencode is currently being implemented right now.

Both Codex CLI and Claude Code can be started in Wisetree's existing PTY with
an initial prompt, an explicit model, and an explicit reasoning/thinking
level. The PTY layer is already generic enough to host either executable.

The main work is not PTY compatibility. It is replacing the OpenCode-specific
turn lifecycle:

- `Develop` starts an OpenCode TUI that stays open after its turn.
- Wisetree detects completion and extracts the assistant transcript by reading
  OpenCode's private SQLite database.
- The workflow parses that transcript, advances to the next state, and may kill
  the still-open TUI.

For a production integration, the recommended approach is to run Codex and
Claude in their documented one-shot modes (`codex exec` and `claude -p`) and
add a provider-neutral run adapter. Process exit then supplies the completion
signal, and documented output formats supply the transcript. This is more
robust than creating two more watchers for private session-file formats.

## What Wisetree does today

The relevant pieces are:

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
  reconstructs the clean assistant transcript because terminal capture is not
  suitable for contract parsing.
- `src/config/schema.rs:114` stores only `{ model, thinking }`, and its model
  value is specifically an OpenCode `provider/model` identifier.

The existing dashboard activity detectors are useful prior art:

- `src/services/ai_status/codex.rs:1` already reads Codex rollout JSONL and
  understands turn start/complete/abort events.
- `src/services/ai_status/claude.rs:1` already reads Claude project
  transcripts and understands user/tool-use/end-turn state.

Those detectors make an interactive proof of concept easier, but they do not
currently return a transcript tied unambiguously to the child Wisetree just
started.

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

These are argument-vector examples, not shell strings. Wisetree should continue
passing prompt text as one argument so backticks, quotes, `$()`, and newlines
cannot be interpreted by a shell.

### Codex interactive

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

This should render inside the current PTY. `--no-alt-screen` is also available
if inline rendering behaves better inside the nested terminal. Like OpenCode,
the interactive TUI remains open after a completed turn, so Wisetree would
still need a Codex-specific completion/transcript watcher.

### Codex recommended one-shot mode

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

This is the cleanest mapping. JSONL provides activity events, the output file
provides the exact final assistant message, and the exit status provides
success/failure. Planning should use `read-only`; implementation needs
`workspace-write`.

The placement above matters. In Codex CLI 0.145.0,
`codex exec --ask-for-approval never` is rejected because the flag is global
and is not accepted in that post-subcommand position. The parser accepts
`codex --ask-for-approval never exec ...`, but
`--config approval_policy="never"` is accepted directly by `exec` and maps to
the documented non-interactive configuration. It does not grant extra access:
the sandbox still limits the run, and operations that need unavailable
approval fail. Wisetree should not use
`--dangerously-bypass-approvals-and-sandbox`.

### Claude Code interactive

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

This also should render in the current PTY and lets the user answer permission
prompts while the inner terminal is focused. As with interactive Codex and
OpenCode, Wisetree needs a provider-specific watcher to know when the turn,
rather than the process, has finished.

### Claude Code recommended one-shot planning

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

`--permission-mode plan` is not the recommended one-shot planning policy.
Although it is read-only, Claude can call `ExitPlanMode` when the plan is ready
or `AskUserQuestion` while preparing it. Claude's hooks reference says both
normally block under `-p` unless the caller implements an interaction hook.
`dontAsk` plus an explicit read-only `--tools` list is a smaller, reliably
non-interactive contract. The generated prompt should tell Claude to return
the plan as its final response and not request interaction.

If planning must use read-only shell commands such as `git diff`, add `Bash`
to `--tools` only together with narrowly scoped `--allowedTools` rules. Do not
allow arbitrary Bash merely to reproduce interactive plan mode.
`--disallowedTools mcp__*` is necessary because `--tools` restricts built-in
tools but does not restrict MCP tools.

### Claude Code recommended one-shot implementation

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

Here `Bash(<exact-check-command>)` is one argument containing a Claude
permission rule, derived from the configured Develop check command. Add other
rules only for commands Wisetree intentionally authorizes. `acceptEdits`
allows file edits without prompting; unapproved commands can still be denied
instead of silently broadening access. Wisetree should never substitute
`--dangerously-skip-permissions`.

For both one-shot forms, the stream carries activity and ends with a `result`
message, and the process exits after the turn. Unlike Codex, Claude does not
document a final-message file option, so Wisetree must capture and parse the
stream itself.

Claude's docs also record important stream-version fixes: before 2.1.208 a
large piped response could omit the final `result`, and before 2.1.214 the
short output-drain wait could truncate the end of a large response. The local
2.1.198 binary accepts these flags but predates both fixes. Require Claude Code
2.1.214 or newer for the streamed adapter, or temporarily use
`--output-format json` without live token streaming during a spike.

## Required design changes

### 1. Add harness to the persisted per-command configuration

The proposed Settings shape is sound. Keep one canonical `provider/model`
value in the configuration and add a typed harness:

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
`claude_code`. A separate three-value execution enum keeps the persisted
command contract narrow. Suggested JSON values are `opencode`, `codex`, and
`claudeCode`; the UI label for the last value can be `claude code`.

Add `#[serde(default)]` to `AiModelConfig.harness` and make
`AiCommandHarness::default()` return `Opencode`. That gives all of these cases
the required behavior:

- existing nested `{ model, thinking }` values become OpenCode;
- the older flat AI configuration migrated by `AiConfig::deserialize` becomes
  OpenCode for all twelve slots;
- absent per-command slots use their existing model/thinking defaults plus
  OpenCode;
- newly saved values serialize the harness explicitly.

The model stays canonical in storage because it is also what the existing
model picker returns. Translate only at the launcher boundary:

| Harness | Accepted stored provider | CLI model argument |
| --- | --- | --- |
| OpenCode | Any provider | Pass the complete `provider/model` value |
| Codex | Exactly `openai` | Strip `openai/` and pass the model slug |
| Claude Code | Exactly `anthropic` | Strip `anthropic/` and pass the model ID |

Do not infer provider from the model's marketing name. For example,
`github-copilot/gpt-*` is not an `openai` provider under this rule and
therefore offers only OpenCode.

The valid harness list for a row is consequently:

```text
openai/*    -> [opencode, codex]
anthropic/* -> [opencode, claudeCode]
everything  -> [opencode]
```

If the model is changed and the current harness becomes incompatible, reset
the harness to OpenCode and mark the row modified. A hand-edited configuration
that explicitly combines an incompatible provider and harness should fail
validation with the slot path and accepted choices; it must not silently run a
different executable.

### 2. Implement the proposed AI Models interaction

The screen is already `Settings -> Dashboard -> ai`, backed by
`AiSettingsEditor`. Change the description to:

```text
Pick a model + thinking strength + harness per AI command:
```

Render each non-empty row as three independently styled spans:

```text
openai/gpt-5.6-sol  ·  medium  ·  opencode
```

Add a small field-focus value to the editor:

```text
AiSettingsField = Model | Thinking | Harness
```

`AiSettingsSelection` should continue to own vertical location
(`Rect(index)`, `FreeModels(index)`, or `Save`); `AiSettingsField` owns the
horizontal focus only while a rectangle is selected. This is less invasive
than multiplying every selection variant by three and preserves the existing
scroll window, free-model row, and Save navigation.

Recommended keyboard contract:

| Location | Key | Behavior |
| --- | --- | --- |
| Command row | Up/Down or `j`/`k` | Move between command rows, preserving the focused column |
| Command row | Left/Right or `h`/`l` | Move focus between Model, Thinking, and Harness; clamp at the ends |
| Model focused | Enter | Open the model picker |
| Thinking focused | Space | Advance through Default plus the supported levels, circularly |
| Harness focused | Space | Advance through the compatible harness list, circularly |
| Free-model row | Left/Right, Enter | Keep the existing chip-cycle and stage behavior |
| Save | Enter | Persist the complete Dashboard configuration |

Space on Model and Enter on Thinking/Harness should be inert. This keeps one
obvious key for each action. The footer should say:

```text
Up/Down move · Left/Right choose field · Space change value · Enter pick model/Save · Esc back
```

Today `render_ai_settings_rectangle` applies bold white styling to the whole
line and dims only the thinking suffix. Change it to style the focused span
white + bold and the other two spans muted/dim, leaving separators muted.
Default focus when the page opens should be Model. The rectangle border and
Saved/Modified colors can remain unchanged.

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
settings file. Legacy entries have `Opencode` through the serde default, so
that is what the new column displays.

Widen the centered table enough for the new fixed-width column and let Model
remain the flexible/clipped column on narrow terminals. The table's height is
unchanged, but its render tests and all `AiRoleRow::new` call sites must be
updated.

The current model picker also asks for a thinking variant after model
selection. With inline Space handling, that second picker phase becomes
duplicative and, for Codex/Claude, can use the wrong harness's variants.
Simplify `AiModelPickerAction::Selected` to return the model only. On return:

- preserve the current thinking value if it is supported by the current
  harness/model pair;
- otherwise reset thinking to Default;
- reset an incompatible harness to OpenCode as described above.

The free OpenCode model chips need the same normalization because selecting an
`opencode/*` chip while Codex or Claude is selected makes that harness invalid.

Mouse support currently exists for the AI Save button but not individual
command-row fields. Keyboard behavior is sufficient for this feature; if row
mouse targets are added later, each span needs its own hit rectangle so a
click selects the correct field rather than opening the model picker
unconditionally.

### 3. Make thinking choices harness-aware

The current variant map is keyed only by `provider/model` and comes from
`opencode models --verbose`. It cannot be reused as the source of truth for
other harnesses. Resolve choices by `(harness, provider/model)`:

- OpenCode: keep the existing per-model variant map and generic fallback.
- Codex: parse `codex debug models --bundled`, whose JSON includes each model
  slug's `supported_reasoning_levels`. The installed 0.145.0 binary exposes
  this command without making a paid model call.
- Claude Code: the CLI documents `--effort` and prints the levels supported by
  that CLI version, but does not document an equivalent per-model catalogue.
  Use the locally advertised levels as a best-effort list, retain Default, and
  treat model-specific acceptance as a launch preflight. Do not hardcode an
  OpenCode-derived ladder as authoritative for Claude.

For context, the generic Codex config reference lists
`minimal|low|medium|high|xhigh`, while the installed Codex catalogue offers
`low|medium|high|xhigh|max|ultra` for `gpt-5.6-sol`. Installed Claude Code
2.1.198 advertises `low|medium|high|xhigh|max`; current Claude documentation
also describes `ultracode` on newer versions and supported models. One shared
ladder would therefore accept invalid combinations or hide valid ones.

When Space changes the harness, immediately re-resolve the thinking list. Keep
the current value if it remains valid; otherwise reset it to Default in the
same row mutation. This prevents saving a thinking level that was valid for
OpenCode but invalid for Codex or Claude.

### 4. Add a provider-neutral run contract

Replace `FixApplyHandoff { opencode_binary, opencode_args, cwd }` and
`OpencodeTurnWatcher` at the workflow boundary with concepts such as:

```text
AiRunSpec {
    harness,
    binary,
    args,
    cwd,
    output_mode,
}

AiRunEvent = Activity | Finished { transcript } | Failed { message }
```

OpenCode can initially keep its existing adapter. Codex and Claude adapters
should use their documented one-shot output.

### 5. Route every configured AI phase through the selected harness

The harness is stored on all twelve `AiModelConfig` leaves, not only Develop.
The current execution paths are:

| Settings slot | Current OpenCode behavior |
| --- | --- |
| `explain` | Interactive TUI + private-database turn watcher |
| `fix.plan` | Captured `opencode run`, then parse structured text |
| `fix.apply` | Interactive TUI + turn watcher |
| `review.strong/balanced/utility` | Multiple captured `opencode run` calls |
| `update` | Interactive TUI + turn watcher for PR and local-branch conflicts |
| `bugkill.investigate` | Interactive TUI + turn watcher |
| `bugkill.fix` | Interactive TUI + turn watcher |
| `bugkill.judge` | Captured `opencode run` |
| `develop.plan` | Interactive TUI + turn watcher |
| `develop.implement` | Interactive TUI + turn watcher |

Every call site must request an `AiRunSpec` from a harness adapter rather than
reading a global `opencode_binary`. In particular:

- binary availability checks must inspect the selected slot's executable;
- `FixApplyHandoff`, `ConflictsHandedOffToUi`, and similar outcomes must carry
  a provider-neutral run specification;
- the five `OpencodeTurnWatcher` fields in `App` must become run-owned
  lifecycle/capture state for one-shot Codex and Claude runs;
- screen methods and labels such as `spawn_opencode_pty`, `Focus opencode`, and
  `Launching opencode...` must use the selected harness display name;
- `PrConfirmView` must add Harness as the last column after Role, Model, and
  Thinking, populated from the already-resolved per-role configuration;
- Review benchmark/capture provenance should include harness so results from
  different executables cannot be compared as if they were identical.

This creates an important rollout rule: do not display Codex or Claude for a
slot until that slot's complete execution path supports the adapter. If the
first implementation covers only `develop.plan` and `develop.implement`, the
harness list for the other ten slots must temporarily remain `[opencode]`.

The executable names are `opencode`, `codex`, and `claude`. Keep the product
labels `OpenCode`, `Codex CLI`, and `Claude Code` separate from binary names.
At launch, convert the stored canonical model as described above, build the
harness-specific arguments, and return an actionable error naming the missing
binary, unsupported model, or unsupported effort.

### 6. Preserve the existing local/global save semantics

No new persistence path is needed. For an AI-only edit, the existing flow
already does what is required:

1. AI Settings Save returns `SettingsAction::SaveDashboard`.
2. `App::save_dashboard` writes the whole dashboard to the existing
   project-local `.wisetree.json` when that file exists.
3. If no local file exists, it writes `~/.wisetree/settings.json`.
4. It reloads the active `ConfigService` and marks the Dashboard editor saved.

Esc currently stages AI edits back into the Dashboard editor but does not
write them. Preserve that behavior; only Enter on the AI page's Save button
persists. Add tests for both target paths with a non-default harness, as the
existing tests already cover the local/global routing for Dashboard settings.

One existing exception deserves a regression test: `App::save_dashboard`
forces a project-local write when `wiseMerge` changed, even if no local file
previously existed. If a user stages `wiseMerge`, enters AI Settings, and saves
there, that special case currently wins over the global fallback. Either keep
that established Dashboard behavior and document it, or ensure AI Settings
Save cannot accidentally include an unrelated staged `wiseMerge` change. The
harness field itself must not create a local file when none exists.

### 7. Capture output independently of terminal rendering

`PtyView` currently sends bytes only to the `vt100` parser. Extend the reader
to tee raw bytes/events to a bounded capture or channel.

- Codex can render its normal progress in the panel and read the final message
  from `--output-last-message`; alternatively, parse `--json`.
- Claude stream-JSON should be parsed into a small provider-neutral activity
  view rather than displayed as raw JSON lines.
- Keep a strict memory cap and retain only the final transcript plus a bounded
  activity tail.

This is the largest UI difference. Running the interactive TUIs gives the
closest visual parity with OpenCode; running the supported one-shot modes gives
the cleanest lifecycle and output contract.

### 8. Separate planning and implementation permissions

The current OpenCode `--agent plan` is semantically important, not just a UI
choice.

- Codex planning: `--sandbox read-only`.
- Codex implementation: `--sandbox workspace-write`.
- Claude interactive planning: `--permission-mode plan`.
- Claude one-shot planning: `--permission-mode dontAsk` with a strict read-only
  `--tools` list; do not expose `ExitPlanMode` or `AskUserQuestion` unless
  Wisetree implements their interaction hook.
- Claude implementation: `--permission-mode acceptEdits` with an explicit tool
  list and narrowly scoped `--allowedTools` rules for configured checks.

Wisetree should show the effective permission policy on the confirmation page
alongside harness, model, and thinking level.

### 9. Add capability checks

Checking only `<binary> --version` is not enough. Versions and entitlements can
differ, and effort levels are model-dependent. Preflight should report:

- binary absent;
- not authenticated;
- requested model unavailable;
- requested effort unsupported;
- permission/output flag unsupported by the installed CLI version.

The repository already has both binaries installed for a local spike:
Codex CLI 0.145.0 and Claude Code 2.1.198. Their local `--help` output confirms
the prompt/model/effort and structured-output flags described above. No paid
model invocation was made during this investigation. Local parsing also
confirmed that Codex rejects `--ask-for-approval` after `exec` and accepts
`--config approval_policy="never"`. Claude 2.1.198 is adequate for argument
construction tests, but should be upgraded before relying on streamed final
output.

### 10. Code areas and verification

The implementation is wider than the Settings row itself:

- `src/config/schema.rs`: add `AiCommandHarness`, the defaulted field, legacy
  migration, validation, defaults, JSON Schema output, and serialization tests.
  Every `AiModelConfig` struct literal in source/tests must either set harness
  or use a constructor/default update.
- `src/tui/screens/settings.rs`: add horizontal field focus, Space cycling,
  provider filtering, harness-aware thinking lookup, per-span styling,
  normalization, help text, and local editor tests.
- `src/tui/screens/ai_model_picker.rs` and
  `src/services/opencode_models.rs`: make model selection independent of the
  OpenCode-only variant phase, and introduce harness-aware capability sources.
- `src/services/dashboard.rs`: replace hard-coded OpenCode binary gates,
  argument construction, output parsing, handoff types, and messages with
  adapters. Preserve the existing prompt builders and deterministic parsers.
- `src/tui/app.rs`: resolve the correct binary, own structured child output,
  and replace OpenCode-only watchers where one-shot mode is used.
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
- stub Codex/Claude processes exercise structured progress, final transcript,
  non-zero exit, malformed output, timeout, and cancellation;
- confirmation views show the selected harness for every role.

`dashboard.aiStatus.enabledHarnesses` is a separate feature: it controls
background detection of arbitrary external AI sessions. Selecting Codex or
Claude for a command should not silently rewrite that monitoring preference.
The embedded AI Activity panel must track its own child regardless of the
background detector setting.

## Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Reading private session files | CLI updates can break completion or transcript parsing | Prefer documented one-shot output; keep existing detectors only for dashboard status |
| Same-cwd session races | A watcher can attach to a user's unrelated session | Use child-owned stdout/session ID/output file, not "newest session in cwd" |
| Model ID mismatch | Existing OpenCode values are invalid in Codex/Claude | Add `harness` and validate models per harness |
| Effort mismatch | Levels vary by model and CLI version | Discover capabilities and allow "default" |
| Harness exposed before its slot is implemented | Settings saves Codex/Claude but runtime still launches or watches OpenCode | Gate each slot's harness choices on completed adapter coverage |
| Model change leaves stale harness/effort | Saved combination is invalid at launch | Normalize both harness and thinking immediately after model or harness changes |
| Unattended permission prompt | Run stalls or fails midway | Use explicit non-interactive policies; avoid Claude `plan` mode under `-p` unless an interaction hook handles `AskUserQuestion` and `ExitPlanMode` |
| Truncated Claude stream | Older Claude versions can omit part or all of the final `result` | Require Claude Code 2.1.214+ for stream-JSON, or use single-result JSON during the spike |
| Raw JSON in the PTY | Poor AI Activity UX | Parse structured streams into provider-neutral activity rows |
| Nested TUI differences | Mouse, alternate screen, resize, or key handling may differ | Spike interactive rendering on macOS/Linux; test Codex with and without `--no-alt-screen` |
| Authentication/subscription differences | Binary exists but cannot run the selected model | Add an auth/model preflight and actionable error text |
| Instruction-file differences | Harnesses may load different project/user instructions | Keep workflow-critical constraints in the generated prompt and document harness-specific instruction loading |

## Recommended rollout

1. Add the defaulted harness schema, provider validation, model translation,
   and Settings sub-focus/Space behavior. Keep every slot's visible harness
   list at `[opencode]` until its runtime adapter is ready.
2. Introduce `AiRunSpec` and structured run completion, then implement
   `develop.plan` and `develop.implement` with `codex exec`. Enable Codex only
   for those two OpenAI-backed slots.
3. Add Claude Code 2.1.214+ stream parsing and explicit planning/implementation
   permission policies. Enable it for those two slots only when their model
   provider is Anthropic.
4. Extend the same adapters to the captured-output phases first
   (`fix.plan`, Review, `bugkill.judge`), then the remaining interactive
   handoffs (Explain, Fix apply, Update, Bugkill).
5. Add Harness to confirmation tables and provenance, convert remaining
   OpenCode-specific UI strings, and then enable all compatible choices.
6. Keep interactive OpenCode behavior initially. Consider moving it to
   `opencode run` later only if a single event-rendering UI across all
   harnesses is worth the behavior change.

## Final assessment

- **Can Codex CLI run in the inner PTY?** Yes.
- **Can Claude Code run in the inner PTY?** Yes.
- **Can both receive prompt, model, and thinking/effort at launch?** Yes.
- **Is this a command-line substitution only?** No. The current completion and
  transcript path is OpenCode-specific.
- **Will the proposed Settings interaction work?** Yes. It fits the existing
  `AiSettingsEditor` cleanly and the existing Save path already has the desired
  local/global behavior.
- **Is it practical to implement?** Yes. Develop-only support is moderate;
  correctly supporting the harness selector across all twelve slots is a
  larger cross-cutting change because the current runtime and UI lifecycle are
  OpenCode-specific.
- **Best first target:** Codex one-shot mode on the Develop page, followed by
  Claude one-shot mode once its structured stream and permission policy are
  represented cleanly.

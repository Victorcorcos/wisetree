# 🎛️ Worktree Presets — Implementation Plan Prompt

You are implementing a new configurability feature in the `wisetree` Rust + Ratatui codebase: **named worktree presets**. Today, every `wisetree create` invocation uses the same global config — same copy patterns, same post-create commands, same terminal launcher. That's wrong for multi-purpose repos: a quick bugfix worktree shouldn't have to wait through `bin/rails db:prepare`, while a full feature worktree should.

A preset bundles a *partial override* of `WorktreeConfig` under a name. Users select a preset during create (TUI) or via `--preset <name>` (CLI). The base config and the preset are merged, with preset fields taking priority. When no preset is selected, behavior is identical to today.

Treat this as a self-contained brief. Match the project conventions described below; do not invent new ones. When in doubt, mirror the patterns already established by `ConfigService` and `WorktreeConfig`.

---

## 1. Goal & Scope

**In scope**
- New `presets: HashMap<String, PresetConfig>` field on `WorktreeConfig`.
- `PresetConfig` schema: a partial `WorktreeConfig` where every field is optional, plus optional metadata (`description`, `inheritsFrom`, `terminalCommand`, `agent`).
- Resolution: `effective_config = base.merge(preset)`, with preset overriding non-`None` fields.
- TUI integration: a new "Pick preset" step in the create flow, between the directory-name and source-branch prompts, shown only when ≥1 preset is configured.
- CLI integration: `--preset <name>` flag on `wisetree create`, and `wisetree presets [list|show <name>|use <name>]` subcommand.
- Settings screen: a read-only "Presets" detail view listing each preset and its overrides.
- Tests: schema, merge logic, TUI flow, CLI parsing, end-to-end.

**Out of scope (explicitly defer)**
- Editing presets from the TUI. Users edit `.wisetree.json` directly in v1.
- Per-branch automatic preset selection (e.g. "branches matching `hotfix/*` use the hotfix preset"). That's a separate feature once preset resolution is solid.
- Inheritance chains deeper than one level. `inheritsFrom` resolves once; circular references are detected and rejected at load.

---

## 2. Configuration changes

Extend `WorktreeConfig` in `src/config/schema.rs`:

```rust
/// Named partial overrides selectable at create time. Empty by default.
/// Keys are user-chosen names (e.g. "frontend", "hotfix"); values are
/// partial configs that win against the base when selected.
#[serde(rename = "presets", default)]
pub presets: BTreeMap<String, PresetConfig>,
```

Use `BTreeMap` (not `HashMap`) so preset ordering in the TUI selection list is deterministic and the JSON output is stable. Keys must match `^[a-zA-Z][a-zA-Z0-9._-]*$` — validate at load and reject invalid names with a clear error.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct PresetConfig {
    /// Human-readable description shown in the TUI selector. Single line.
    #[serde(rename = "description", default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Inherit from another preset by name. Resolved exactly once at load.
    /// Cycles are rejected. Unknown names are rejected.
    #[serde(rename = "inheritsFrom", default, skip_serializing_if = "Option::is_none")]
    pub inherits_from: Option<String>,

    /// Override `worktreeCopyPatterns`. None = inherit from base.
    #[serde(rename = "worktreeCopyPatterns", default, skip_serializing_if = "Option::is_none")]
    pub worktree_copy_patterns: Option<Vec<String>>,

    /// Override `worktreeCopyIgnores`. None = inherit from base.
    #[serde(rename = "worktreeCopyIgnores", default, skip_serializing_if = "Option::is_none")]
    pub worktree_copy_ignores: Option<Vec<String>>,

    /// Override `worktreePathTemplate`. None = inherit from base.
    #[serde(rename = "worktreePathTemplate", default, skip_serializing_if = "Option::is_none")]
    pub worktree_path_template: Option<String>,

    /// Override `postCreateCmd`. None = inherit from base.
    #[serde(rename = "postCreateCmd", default, skip_serializing_if = "Option::is_none")]
    pub post_create_cmd: Option<Vec<String>>,

    /// Override `terminalCommand`. None = inherit from base.
    #[serde(rename = "terminalCommand", default, skip_serializing_if = "Option::is_none")]
    pub terminal_command: Option<String>,

    /// Override `deleteBranchWithWorktree`. None = inherit from base.
    #[serde(rename = "deleteBranchWithWorktree", default, skip_serializing_if = "Option::is_none")]
    pub delete_branch_with_worktree: Option<bool>,
}
```

Important nuances:
- **Lists are replaced, not appended.** A preset's `postCreateCmd` is the *full* list to run. Concatenation semantics ("append to base") feel intuitive but produce confusing emergent behavior; explicit replacement is easier to reason about. Document this loudly in the README.
- **Empty list ≠ unset.** `postCreateCmd: []` in a preset clears the base list. `postCreateCmd: None` (i.e. omitted) inherits the base list. The `Option<Vec<_>>` shape makes this distinction explicit at the type level.
- **`inheritsFrom` resolves once.** Resolution order: parent fields fill in the child where the child is `None`, then the merged result overlays the base. No transitive inheritance.

Add this all to `tests/config.rs`: round-trip serialization, name validation, cycle detection, unknown-parent rejection, and `deny_unknown_fields` on `PresetConfig`.

---

## 3. Resolution logic

Create `src/config/preset.rs` with:

```rust
/// Returns the effective `WorktreeConfig` for the given preset, or the
/// base config unchanged when `name` is `None`. Errors when `name` is
/// `Some(_)` but missing from `base.presets`.
pub fn resolve(base: &WorktreeConfig, name: Option<&str>) -> Result<WorktreeConfig>;

/// Merge `partial` into `base`, returning a new `WorktreeConfig`. Each
/// `Some(value)` in `partial` overwrites the corresponding base field.
pub fn merge(base: &WorktreeConfig, partial: &PresetConfig) -> WorktreeConfig;

/// Validate the preset map at config-load time: name regex, no cycles,
/// `inheritsFrom` targets must exist.
pub fn validate(presets: &BTreeMap<String, PresetConfig>) -> Result<()>;
```

`validate` runs from inside `ConfigService::load_from_path` after deserialization. Errors are surfaced through the existing `WisetreeError::config(...)` constructor.

`resolve` is the single chokepoint that every consumer (TUI, CLI, automation) calls. There is exactly one place where presets become real, and that place is `resolve`. Do not sprinkle merging logic across consumers.

---

## 4. TUI integration

The create flow lives in `src/tui/screens/create.rs`. Extend the `CreateStep` enum with a `PickPreset` step inserted at the start of the flow when `config.presets` is non-empty. When empty, skip directly to the existing first step — current users see no UI change.

Step order with presets configured:
1. **PickPreset** — `SelectPrompt` titled `"Select a preset (or skip)"`. First option is always `"(no preset)"`. Remaining options are presets sorted by key, each rendering `name — description` when description is set, otherwise just `name`. `Esc` returns to the previous screen; `Enter` confirms.
2. DirectoryName (existing).
3. SourceBranch (existing).
4. NewBranch (existing).
5. Confirm (existing).
6. Creating (existing).

Selecting a preset stores the chosen name in `CreateScreen::preset_name: Option<String>`. The `App` (in `src/tui/app.rs`) calls `preset::resolve(base_config, preset_name.as_deref())` exactly once before kicking off `WorktreeService::create_worktree`, and passes the resolved config in via the existing `ConfigService::update` mechanism (or by extending `WorktreeService::create_worktree` to accept an explicit `&WorktreeConfig`, whichever is cleaner — favor an explicit parameter).

The `Confirm` step's summary panel shows the active preset name when set, so the user sees `Preset: hotfix` alongside `Source: main`, `Branch: feat/payments`, etc. Use the `messages::colors::ACCENT` token for the value.

---

## 5. CLI integration

Two surface additions:

**`wisetree create --preset <name>`**
- `src/cli/args.rs`: add `preset: Option<String>` to `CliArgs`. Recognize `--preset <name>` and `--preset=<name>`. No short alias.
- `src/cli/commands/create.rs`: resolve the preset via `preset::resolve` and pass the result down.
- Unknown preset → exit non-zero with `WisetreeError::config(format!("Unknown preset: {name}"), Some(config_path))`.
- Update `help_text()` so the non-interactive options table includes `--preset`.

**`wisetree presets`** subcommand:
- `wisetree presets list` — print one line per preset: `name — description (overrides: copyPatterns, postCreateCmd, ...)`.
- `wisetree presets list --json` — emit the entire presets map verbatim.
- `wisetree presets show <name>` — print the resolved (post-`inheritsFrom`) `PresetConfig` and the effective `WorktreeConfig` after merge.
- `wisetree presets use <name>` — alias for `wisetree create --preset <name>`. Routes through the existing create flow, interactive or not depending on other flags.

Wire-up:
- `src/cli/args.rs`: add `AppMode::Presets`, `CliCommand::Presets { action: PresetAction }`. `PresetAction { List { json: bool }, Show { name: String }, Use { name: String } }`.
- `src/cli/commands/presets.rs`: new module dispatching the action.
- `src/cli/run.rs`: route the new subcommand.
- `src/cli/commands/mod.rs`: re-export.

`--mode presets` is a valid TUI mode that opens the menu's Settings → Presets view directly.

---

## 6. Settings screen integration

`src/tui/screens/settings.rs` already exposes per-field detail views via `SettingsStep`. Add `SettingsStep::Presets`. The view renders:

- One row per preset: `name — description`.
- For the highlighted preset, a sub-panel listing its overrides field-by-field, with `(inherits)` for `None` fields.
- `Enter` on a preset shows the merged effective config (read-only).
- No editing in v1 — the footer states "Edit `.wisetree.json` to change presets."

---

## 7. Errors

Reuse `WisetreeError`. Add no new variants — `WisetreeError::config(message, path)` covers every preset-related failure (unknown name, invalid name, cycle, missing parent). Each error message must include the preset name and, when applicable, the parent name.

Validation order at load:
1. Each key matches the name regex. Failure → `"Invalid preset name: <name>"`.
2. Each `inheritsFrom` target exists. Failure → `"Preset '<child>' inherits from unknown preset '<parent>'"`.
3. No cycles. Failure → `"Preset inheritance cycle detected: a -> b -> a"`. Use a depth-first walk with a visited-set to detect.

---

## 8. Tests

- `tests/config_preset.rs` (new):
  - `merge` overwrites only `Some(_)` fields; `None` preserves base.
  - `merge` of empty list clears the base list (replacement, not append).
  - `resolve(base, None)` returns the base unchanged.
  - `resolve(base, Some("missing"))` returns the expected error.
  - `inheritsFrom` resolves one level; child overrides win over parent overrides.
  - Cycle detection: `a -> b -> a` is rejected.
  - Name regex: empty, leading-digit, and special-char names rejected.
- `tests/config.rs`:
  - Full round-trip including non-empty `presets`.
  - `deny_unknown_fields` on `PresetConfig`.
  - Stable JSON ordering (BTreeMap).
- `tests/tui_create.rs`:
  - With zero presets, the flow is unchanged (first step is still DirectoryName).
  - With ≥1 preset, the flow starts at PickPreset.
  - Selecting `(no preset)` produces `preset_name: None` and continues.
  - Selecting a preset stores the name and continues.
  - The Confirm summary shows `Preset: <name>` when set.
- `tests/cli_args.rs`:
  - `--preset hotfix` parses into `CliArgs::preset = Some("hotfix")`.
  - `wisetree presets list` parses to `CliCommand::Presets { action: List { json: false } }`.
  - `wisetree presets show <name>` parses correctly; missing name surfaces a clear error.
- `tests/cli_e2e.rs`:
  - With a fixture config containing one preset, `wisetree create -n test -s main --preset frontend` produces a worktree whose post-create commands ran with the preset's list, not the base list. (Use a no-op echo command to verify — capture stdout.)

Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all` before declaring done.

---

## 9. Documentation

Update the README:
- Add `presets` to the configuration table.
- Add a new subsection under "Configuration" titled **"Presets"** with:
  - A worked example showing two presets (`hotfix` and `feature`).
  - The merge rules (replacement, not append; empty list clears base).
  - The `inheritsFrom` semantics.
- Add `--preset <name>` to the non-interactive options table.
- Add the `wisetree presets` subcommand to the commands table and CLI examples block.

Re-run the schema generator (`cargo run --bin generate-schema`) so `schema.json` reflects the new shape.

---

## 10. Acceptance criteria

1. With `presets` empty, `wisetree create` behavior is byte-for-byte identical to before — the PickPreset step is skipped.
2. With ≥1 preset, the TUI shows a preset selector as the first create step, and the chosen preset overrides the matching base fields.
3. `wisetree create --preset <name>` works non-interactively, surfaces a clear error for unknown names.
4. `wisetree presets list/show/use` work as documented.
5. `inheritsFrom` resolves one level; cycles are rejected at config load with a clear error.
6. The settings screen renders presets read-only.
7. `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test --all` are clean.

---

## 11. Things to watch out for

- **List replacement, not append.** Reviewers will ask "shouldn't `postCreateCmd` extend the base?" Hold the line — explicit replacement is easier to reason about and easier to undo. Document the choice in the README so users don't have to discover it the hard way.
- **Don't merge eagerly at load.** `ConfigService` should keep the raw `WorktreeConfig` (with the presets map intact). `resolve` is called at the moment of use. Eager merging hides the original config from `wisetree presets show` and `wisetree settings`.
- **Validate at load, not at use.** Cycle detection and name validation belong in `validate`, called from `load_from_path`. By the time `resolve` runs, the map is known good.
- **Stable ordering matters.** `BTreeMap` over `HashMap` — the TUI selector should not reshuffle on each invocation, and `wisetree presets list --json` output should be deterministic for diffing.
- **Don't introduce a separate file format.** Presets live inside the existing `.wisetree.json`. A `~/.wisetree/presets/` directory is tempting but doubles the discovery rules and breaks team-shared configs.
- **`Option<Vec<T>>` is awkward to serialize prettily.** That's fine — `skip_serializing_if = "Option::is_none"` keeps the on-disk shape clean. Don't substitute a custom enum for clever empty/unset distinctions; the option is the simplest correct thing.
- **Preset names are user-facing identifiers.** Treat them as data, not code. Don't lowercase/uppercase them, don't strip whitespace, don't auto-rename. Round-trip them verbatim.

---

## 12. Design guidance

In case something need to be done in TUI, always remember to follow the design and color pallete we already have, documented in:

* design/pallete.md

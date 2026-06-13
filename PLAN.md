## Task Description

Add opt-in terminal-bell notifications for dashboard status transitions. The first two notification scenarios are AI work completion (`AI Status` moves from `Running` to `Finished`) and PR check completion (`PR Checks` moves to green / passed). Notification preferences must be configured through the existing dashboard settings flow, resolved through the current local-then-global config precedence, and disabled by default. The config keys will use the requested `...Ok` naming pattern: `dashboard.notifications.aiStatusOk` and `dashboard.notifications.prChecksOk`.

The implementation will keep notification delivery best-effort by writing ASCII BEL (`\x07`) to the controlling TTY so the user's terminal emulator can beep, flash, bounce, or otherwise alert even when the Wisetree tab/window is not focused.

**Complexity**: 5 points

---

## Implementation Sections

#### Section 1 — Dashboard Notification Config
**Goal**: Add persistent dashboard notification settings with safe defaults and without breaking existing configs.
**Files**: `src/config/schema.rs`, config/schema-related tests in the same module or nearby existing test modules
**Acceptance criteria**:
- [ ] `dashboard.notifications.aiStatusOk` deserializes as a boolean and defaults to `false` when omitted.
- [ ] `dashboard.notifications.prChecksOk` deserializes as a boolean and defaults to `false` when omitted.
- [ ] Existing dashboard config fields and `aiStatus` settings are preserved when notification settings are present or absent.
- [ ] Targeted config/schema tests pass after this section.
**Edge cases**:
- [ ] Old configs without `dashboard.notifications` continue to load.
- [ ] Partial `dashboard.notifications` objects default missing notification keys to `false`.
- [ ] Unknown config keys still follow the existing `deny_unknown_fields` behavior.

---

#### Section 2 — Dashboard Settings Toggles
**Goal**: Surface `aiStatusOk` and `prChecksOk` as togglable options in `3. Settings` → `2. Dashboard` under a clear Notifications grouping.
**Files**: `src/tui/screens/settings.rs`, affected settings-screen tests
**Acceptance criteria**:
- [ ] The dashboard settings editor displays notification options as boolean toggle rows.
- [ ] Pressing Enter on each notification row toggles its staged value before Save.
- [ ] Saving dashboard settings persists the notification toggles using `aiStatusOk` and `prChecksOk`.
- [ ] Saving dashboard settings preserves existing nested dashboard values that are not directly edited, including `aiStatus` configuration.
- [ ] Targeted settings tests pass after this section.
**Edge cases**:
- [ ] Empty or unsaved edits in unrelated dashboard fields do not reset notification settings.
- [ ] Navigating away with Esc leaves persisted notification settings unchanged.
- [ ] The `useAi` picker and free-model chip row remain usable after adding the notification rows.

---

#### Section 3 — Terminal Bell Delivery
**Goal**: Add a reusable best-effort terminal-bell notifier that writes BEL to the controlling TTY.
**Files**: `src/tui/terminal.rs`, terminal helper tests in the same module
**Acceptance criteria**:
- [ ] A public or crate-visible helper can trigger one terminal bell without corrupting stdout.
- [ ] The helper targets the controlling TTY path already used by wrapper mode (`/dev/tty` on Unix, `CONOUT$` on Windows).
- [ ] Failure to open or write to the TTY is ignored or contained so notifications never crash the TUI.
- [ ] Unit tests verify the BEL byte emitted by the pure write path.
**Edge cases**:
- [ ] Running without a controlling TTY does not panic.
- [ ] Wrapper mode continues to keep command-output stdout clean.
- [ ] Multiple notification events can request a bell without leaving terminal state dirty.

---

#### Section 4 — Dashboard Transition Detection
**Goal**: Detect relevant dashboard status transitions and ring the terminal bell only when the corresponding setting is enabled.
**Files**: `src/tui/app.rs`, affected app/dashboard tests
**Acceptance criteria**:
- [ ] `aiStatusOk = true` rings when a worktree's AI status changes from `InProgress` / Running to `Finished`.
- [ ] `aiStatusOk = false` suppresses the same AI transition.
- [ ] `prChecksOk = true` rings when an open PR's checks transition from a known non-passed status to `Passed` / green.
- [ ] `prChecksOk = false` suppresses the same PR checks transition.
- [ ] Initial dashboard load does not ring for rows already in `Finished` or `Passed` states.
- [ ] One dashboard update batch rings at most once, even if multiple rows become OK at the same time.
- [ ] Targeted app/dashboard tests pass after this section.
**Edge cases**:
- [ ] Missing `ai_status`, missing PRs, or missing `checks_status` do not ring.
- [ ] PR checks notifications are based on PR-enriched updates, not git-only refreshes.
- [ ] Dashboard navigation or watch recreation does not produce stale duplicate notifications.

---

#### Section 5 — Final Verification
**Goal**: Run repository checks for the completed feature and update this plan's progress.
**Files**: `PLAN.md` only, unless verification exposes a required fix
**Acceptance criteria**:
- [ ] `cargo fmt --all` passes.
- [ ] Relevant targeted tests pass.
- [ ] `cargo test --all` is run, or any inability to run it is documented.
- [ ] `PLAN.md` progress tracker accurately reflects completed sections.
**Edge cases**:
- [ ] If a full test command is too slow or fails for an unrelated environmental reason, the exact failure is documented.
- [ ] If verification exposes a feature bug, the fix is made in the section where the bug belongs before marking completion.

---

## Progress Tracker

| Section | Name | Status |
|---------|------|--------|
| 1 | Dashboard Notification Config | ☑ Completed |
| 2 | Dashboard Settings Toggles | ☑ Completed |
| 3 | Terminal Bell Delivery | ☑ Completed |
| 4 | Dashboard Transition Detection | ☑ Completed |
| 5 | Final Verification | ☑ Completed |

## Verification Notes

- `cargo fmt --all` passed.
- Targeted tests passed: `cargo test --test config`, `cargo test tui::screens::settings::tests`, `cargo test tui::terminal::tests`, `cargo test tui::app::tests::dashboard_notifications`.
- Initial `cargo test --all` failed only in `tests/wrapper.rs` because the inherited `/Users/victorcorcos/.wisetree/settings.json` contains an unknown `dashboard.wiseMerge` key, so wrapper mode rendered the config error instead of the menu text those tests wait for.
- `HOME="/var/folders/sc/m9xbvr1544xcln40rqv7v6xw0000gn/T/opencode" cargo test --all` passed with an isolated config home.
- `cargo clippy --all-targets -- -D warnings` passed.

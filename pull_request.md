Add push recovery panel for failed PR git pushes

# Description ✍️

When "Update Pull Request" merged cleanly but `git push origin HEAD` failed, users were stranded: the local merge dropped `behind` to 0 (hiding the Update action), and the only feedback was a dead-end toast. Re-triggering Update short-circuited to `AlreadyUpToDate` since `behind == 0`.

This PR adds two complementary features:

1. **`Push Pull Request` action** — appears in the dashboard menu when the PR is Open and the branch is ahead-but-not-behind (`ahead > 0 && behind == 0`), the exact state a failed push leaves behind. Confirms, then runs `git push origin HEAD` via a new push-only service method.

2. **Interactive `Terminal Activity` recovery panel** — on any push failure (clean-merge push, AI commit+push, or the dedicated Push action), the screen hands off to a real shell embedded in the existing `PtyView`. It reproduces the failing push so the user sees the real error. The user can Tab into the shell to run any git command, Tab back out, then **Accept** (re-runs `git push origin HEAD`) or **Esc/Discard** (returns to dashboard). A toast on the dashboard reports the final outcome.

Also hides the misleading "Behind -0" and "(resolving...)" rows on the push-only confirm screen.

# Overview 🔍

### Push Pull Request action flow

| Step | Screen | Description |
|------|--------|-------------|
| 1 | Dashboard | "Push Pull Request" menu item visible when `ahead > 0` and `behind == 0` |
| 2 | Confirm | "Push branch `X` to origin?" prompt with single-step preview (`git push origin HEAD`) |
| 3 | Updating | Push runs — on success: `Pushed` action → toast. On failure: `PushFailed` → terminal recovery |

### Terminal Activity recovery panel layout

```
┌─ Push error ─────────────────────────────┐
│ ! [rejected] HEAD -> main (fetch first)  │
│ error: failed to push some refs to ...   │
└──────────────────────────────────────────┘

┌─ Terminal Activity · outer focused ─────┐
│  $ git push origin HEAD                 │
│  To github.com:me/repo.git              │
│   ! [rejected] HEAD -> main ...         │
│  $ _ (live prompt)                      │
│                                         │
│       │ scrollbar │                     │
└─────────────────────────────────────────┘
Focus: Outer (wisetree)  ·  Tab Switch to shell  ·  ← → Switch button  ·  ↵ Confirm  ·  Esc Discard

  ┌─────────────┐    ┌──────────┐
  │ Accept & Push│    │ Discard  │
  └─────────────┘    └──────────┘
```

### Key design decisions

- **Tail-anchored scrollbar** (`src/tui/widgets/scrollbar.rs`): The shared `render_vertical_scrollbar` utility maps vt100's offset-from-bottom onto ratatui's scrollbar state correctly — the thumb sits flush at the bottom when scrolled to the live tail.
- **Shell scroll uses vt100 scrollback**: Unlike the AI panel (which forwards PageUp/PageDown keystrokes), the terminal panel calls `pty.scroll_up`/`scroll_down` directly since the shell's vt100 keeps a real scrollback buffer.
- **No child-exit gating for recovery panel**: The Accept/Discard buttons are always available regardless of whether the embedded shell has exited — the user can Discard without having to kill the shell first.

# How to Test 🧪

### Preconditions

- A GitHub PR in an **Open** state
- Local branch is ahead of the PR base but **not behind** (`ahead > 0 && behind == 0`) — or you can simulate a push failure via a network-blocked remote or a branch protection rule that rejects pushes

### Happy path — Push Pull Request

1. Open the dashboard for a PR whose branch is ahead-but-not-behind
2. ✅ Verify the **"Push Pull Request"** menu item is visible
3. Press Enter on the menu item
4. ✅ Confirm screen shows "Push Pull Request #N?" with single-step preview ("Will run: git push origin HEAD")
5. Press Enter (Yes)
6. ✅ Toast shows success/failure after push completes

### Terminal recovery — Accept

1. Trigger a push that will fail (e.g. add a commit while remote has diverged, or use a remote that rejects pushes)
2. ✅ "Push error" box appears with the full git rejection message
3. ✅ "Terminal Activity" panel renders below with the embedded shell reproducing the failing push
4. Tab into the shell, run `git fetch` and `git rebase origin/main` (or whatever fix is needed)
5. Tab back out (outer focus restored)
6. ✅ Arrow keys switch between Accept & Push / Discard buttons
7. Press Enter on **Accept & Push**
8. ✅ The re-push runs; toast shows success or the new failure

### Terminal recovery — Discard

1. Repeat steps 1-3 above
2. Press **Esc** (or arrow to Discard and press Enter)
3. ✅ Returns to dashboard; local merge is **intact** (no data loss)
4. ✅ Dashboard may show a warning toast

### Regression checks

- **Normal Update flow still works**: A PR with `behind > 0` shows the standard "Update Pull Request" action (not "Push Pull Request") with the fetch/merge/push step preview
- **AI panel unaffected**: During AI conflict resolution, the AI Activity panel renders as before (no terminal recovery layout)
- **Scrollbar correct at both extremes**: In the terminal panel, fully scroll down → thumb sits flush at bottom. Fully scroll up → thumb at top.
- **Button labels centered**: "Accept & Push" and "Discard" have equal padding on both sides within their button cells

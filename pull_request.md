Add Improve workflow for local review and autonomous fix application
<!-- wisetree-labels: user story 💬 -->

# Description ✍️

Add an "Improve" workflow that reviews the local commit range against the resolved base ref, walks the resulting findings one by one, and lets the user apply each one through the configured Fix AI harness — all without touching GitHub. Each applied checkpoint creates a local commit; failed or cancelled attempts are reverted cleanly.

# Overview 🔍

*Screenshot or video placeholder — the harness injects media here.*

# How to Test 🧪

### Happy path — improve a feature branch
1. Create a feature branch with a few commits on top of `main`
2. Open the Dashboard on that worktree row
3. Press `i` or select **Improve** from the PR command buttons
4. Confirm the dialog — the Review models scan the local diff
5. For each finding, press `Space` to toggle autonomous mode, then `Enter` to apply
6. Verify the AI session opens, edits code, and the screen shows a committed checkpoint
7. After the last finding, press `Enter` on the Done screen to return to the Dashboard

### Manual apply and revision
1. Follow the happy path up to a finding
2. Press `Enter` on **Edit** to adjust severity, title, explanation, or suggestion before applying
3. Press `Enter` on **Feedback** to type revision instructions, then submit

### Edge cases
- **Dirty worktree**: stashing or committing uncommitted changes triggers a `DirtyWorktree` toast — clean up and retry
- **No changes**: an up-to-date branch produces `NoChanges` and drops into Done immediately
- **AI unavailable**: a missing or broken harness shows the `AiUnavailable` toast and aborts the flow
- **Failed commit**: if an AI session exits without producing changes, the attempt is aborted and pre-existing untracked files are restored to their original content
- **Cancelled during preparation**: pressing `Esc` while preparing returns to the Dashboard with no side effects

Prompt to push local commits before squash-merging a PR
<!-- wisetree-labels: user story 💬 -->

# Description ✍️

When a worktree has local commits ahead of its tracking remote, a squash-merge (`gh pr merge --squash`) silently drops them because it only merges what GitHub already has. This adds a "push-before-merge" guard on the Merge confirm screen:

- Detects unpushed local commits during the PR-details fetch (`@{upstream}..HEAD`).
- If no unpushed commits exist, the existing merge-straight-away flow is unchanged.
- If unpushed commits exist, the confirm panel warns with "⚠ X local commits not pushed — a squash-merge drops them unless pushed first."
- Confirming the merge opens a second modal ("Push before merging PR #N?") with **Push & merge** selected by default and **Merge only** as the alternative; Esc aborts.
- `Push & merge` runs `git push origin HEAD` in the worktree and aborts the merge if the push fails.
- `Merge only` proceeds with the squash-merge without pushing (for cases where the user intentionally hasn't pushed yet).

# Overview 🔍

(No screenshot — the change is a new confirmation modal in the merge flow.)

# How to Test 🧪

### Happy path: local commits are pushed before merge

1. From a worktree whose PR is open, add a local commit without pushing.
2. Navigate to Dashboard → select the PR → press `m` to merge.
3. **Expected:** the confirm panel shows a bold warning line with the unpushed commit count.
4. Press Tab to switch to Yes, then Enter.
5. **Expected:** a second modal appears: "Push before merging PR #N?" with "Push & merge" pre-selected.
6. Press Enter.
7. **Expected:** the worktree's HEAD is pushed to `origin`, then the PR is squash-merged.

### Edge case: merge without pushing

1. Follow steps 1–5 above.
2. On the push prompt, press Tab to select "Merge only", then Enter.
3. **Expected:** the PR is squash-merged **without** pushing — local commits stay unpushed.

### Regression: no unpushed commits → no push prompt

1. From a fully-pushed worktree, enter the merge flow.
2. Confirm the merge on the first modal.
3. **Expected:** no push prompt appears; the PR is squash-merged immediately.

### Edge case: Esc on push prompt aborts

1. From a worktree with unpushed commits, reach the push prompt.
2. Press Esc.
3. **Expected:** the merge is cancelled; return to the dashboard.

### Error case: push failure aborts the merge

1. From a worktree with unpushed commits, make `origin` unreachable (e.g. `git remote set-url origin http://bad`).
2. Enter the merge flow and confirm both modals with Enter.
3. **Expected:** a toast shows the push failure; the merge does not proceed.

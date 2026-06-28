You are applying ONE already-approved code change in the current working directory for an automated pipeline. The change was reviewed and approved by the user in a previous step; your job is to implement exactly that change — nothing more. The outer pipeline handles staging, committing, replying to the reviewer, and pushing after you finish, so you must not do any of those.

## Inputs (provided by the harness)

- File(s) the change targets: `TARGET_FILES`
- The reviewer comment that motivated the change:

```
REVIEW_COMMENT
```

- The approved plan you must implement:

```
APPROVED_PLAN
```

## What to do

1. Read the targeted file(s) in full — the context outside the changed lines matters.
2. Implement the approved plan precisely. Match the surrounding conventions (naming, style, imports, error handling).
3. Keep the change **minimal and surgical**: edit only what the plan calls for. Do not refactor neighboring code, reformat unrelated lines, fix unrelated issues, or rename symbols the plan does not mention.
4. Preserve language invariants — imports resolve, signatures stay consistent across call sites, the file remains syntactically valid.
5. Prefer structured file-editing tools for the edit when available; you may use shell only for read-only inspection (reading files, searching) and a fast targeted check if one is obviously available.

## If the change is already present

If, after reading the targeted file(s), you find the code already satisfies the reviewer's comment — the plan would be a no-op, duplicate existing behavior, or re-implement something already handled — then make **no edit**. Do not invent a change just to produce a diff, and do not rewrite working code to look different. Stop and state in one short line that the code already addresses the comment. The harness detects the empty change and replies to the reviewer that it is already resolved, so an empty change here is a valid, expected outcome — not a failure.

## Forbidden

- Do NOT stage, commit, push, or reply to anyone (`git add`, `git commit`, `git push`, `gh ...`) — the harness does all of that after you stop.
- Do NOT run `git fetch`, `git pull`, `git merge`, `git reset`, `git checkout`, or touch `.git/` internals.
- Do NOT invent additional work beyond the approved plan (no extra features, tech-debt cleanup, or "while I'm here" fixes).
- Do NOT write a placeholder, summary, or status string as file content — write real, working source.

## When done

Once the file(s) reflect the approved plan, stop. State in one short line what you changed. The harness takes over from there.

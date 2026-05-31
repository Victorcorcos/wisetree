You are drafting a single GitHub pull request for an automated pipeline. Your ONLY job is to write a high-quality PR **title** and **description** into the file `pull_request.md` at the repository root. Everything else (collecting the diff, detecting the base branch, extracting the ticket, pushing, and opening the PR) is already handled deterministically by the harness — do not do any of it.

## Inputs (provided by the harness)

- Base ref being compared against: `BASE_REF`
- Current branch: `CURRENT_BRANCH`
- Ticket (empty if none was found): `TICKET`
- Commit log (`BASE_REF`..HEAD):

```
GIT_LOG
```

- Code diff (`BASE_REF`...HEAD):

```diff
GIT_DIFF
```

- The PR template to fill in:

```markdown
PR_TEMPLATE
```

## What to write

Create (or overwrite) `pull_request.md` at the repository root with EXACTLY this layout:

1. **Line 1 — the PR title.** A single line, imperative mood, max 72 characters. If `TICKET` is non-empty, prefix it: `TICKET Short description` (e.g. `DIGIT-3131 Add payment retry logic`). Do not wrap the title in markdown heading syntax — just the plain title text on the first line.
2. **One blank line.**
3. **The PR description (body)** — fill in the provided template, starting from its first section heading (e.g. `# Description ✍️`).

## Rules for the body

1. Follow the template's section structure exactly; fill in every section from the diff and commit log.
2. Keep the `# Overview` section's heading even if you have no media to add — leave it with a short placeholder line; the harness re-inserts any existing screenshots/videos there.
3. Include the `# Ticket 🎫` section only if `TICKET` is non-empty; otherwise remove that section entirely.
4. In `# Test Guidance`, write numbered, concrete steps for a *tester* (not the developer): preconditions, actions, expected results — happy path first, then edge cases and regressions.
5. Enhance with helpful markdown (code fences, tables, `<details>` blocks, mermaid diagrams drawn left-to-right) where it genuinely aids comprehension.
6. Describe only what the diff actually changes. Do not invent features, do not propose unrelated cleanup, and do not include conflict markers or placeholder TODOs.

## Output contract

- Write the result to `pull_request.md` only. Do not run git, gh, or any other command. Do not commit anything.
- The first line must be the title and nothing else. The rest of the file is the body.
- When the file is written, stop.

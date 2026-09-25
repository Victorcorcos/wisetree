You are drafting a single GitHub pull request for an automated pipeline. Your ONLY job is to write a high-quality PR **title** and **description** into the file `.wisetree/explain/pull_request.md` relative to the worktree root. Everything else (collecting the diff, detecting the base branch, extracting the ticket, pushing, and opening the PR) is already handled deterministically by the harness: do not do any of it.

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

Create (or overwrite) `.wisetree/explain/pull_request.md` relative to the worktree root with EXACTLY this layout:

1. **Line 1: the PR title.** A single line, imperative mood, max 72 characters. If `TICKET` is non-empty, prefix it: `TICKET Short description` (e.g. `DIGIT-3131 Add payment retry logic`). Do not wrap the title in markdown heading syntax: just the plain title text on the first line.
2. **Line 2: the label comment.** A single `<!-- wisetree-labels: ... -->` comment listing the applicable labels (see **Label selection** below). Example: `<!-- wisetree-labels: bug 🐛, user story 💬 -->`. This line is invisible when the PR is rendered.
3. **One blank line.**
4. **The PR description (body)**: fill in the provided template, starting from its first section heading (e.g. `# Description ✍️`).

## Label selection

Analyse the diff and commit log and choose **all** labels that apply from this list (use the exact strings including emoji):

- `user story 💬`: a new end-user-facing feature or user-journey change
- `bug 🐛`: fixes a defect or incorrect behaviour
- `technical debt 🛠️`: refactoring, cleanup, or internal improvement with no user-visible change
- `documentation 📖`: doc-only changes (README, comments, changelogs)
- `architecture 🏰`: structural/design changes that affect how the system is built
- `security 🛡️`: security hardening, vulnerability fixes, or auth/permission changes

Multiple labels are allowed and encouraged when a PR spans several categories (e.g. a bug fix that also improves architecture). At least one label must always be chosen.

## Rules for the body

1. Write like a teammate explaining a change: plain words, short sentences, active voice. Be concise and easy to understand. Avoid jargon, promotional language, stock introductions, repetition, and a file-by-file recap. Scale the description to the change; a small fix needs only a few sentences.
2. Never use the em dash character (U+2014) anywhere in the generated title or body. Use a period, comma, colon, or parentheses instead. Before writing the file, check the entire draft for this character and rewrite any sentence containing it.
3. Use the template's headings and order, with the section rules below taking precedence over conflicting template instructions. Remove instructional placeholders. Add `# Technical Details 📟` after Overview only when additional code explanations are needed, even if the template does not include it; omit it otherwise.
4. In `# Description`, explain the main change and why it matters in one short paragraph, usually 2 to 4 sentences. Lead with what changes for the user or system. Use a simple before/after example when helpful. Keep implementation details in Technical Details and do not repeat them here.
5. Reserve `# Overview` exclusively for screenshots, videos, GIFs, visualizations, or visual before/after comparisons, with brief captions if needed. Never put technical prose, code explanations, or a second summary here. Use only real media supplied in the inputs, or a simple diagram grounded in the diff when it makes the behavior clearer. Never invent media URLs. Keep the heading and leave the section empty if no useful visual is available; the harness re-inserts existing screenshots/videos there. Add the heading if the template lacks it.
6. In `# Technical Details 📟`, include only code details a reviewer needs to understand a non-obvious implementation decision, constraint, or tradeoff. Use a short paragraph or a few concise bullets. Skip routine implementation steps, exhaustive lists of files or symbols, and details already explained elsewhere.
7. Include the `# Ticket 🎫` section only if `TICKET` is non-empty; otherwise remove that section entirely.
8. In `# Test Guidance`, write a short numbered list for a tester: necessary preconditions, actions, and expected results. Cover the main path and relevant regressions without inventing an exhaustive checklist or claiming tests were run.
9. Prefer plain paragraphs and simple lists. Add richer markdown only when it makes the explanation shorter or clearer; visualizations belong in Overview.
10. Describe only what the diff actually changes. Do not invent features, propose unrelated cleanup, include conflict markers or placeholder TODOs, or omit important limitations just to shorten the text.

## Output contract

- Write the result to `.wisetree/explain/pull_request.md` only. Do not run git, gh, or any other command. Do not commit anything.
- The first line must be the title and nothing else. The rest of the file is the body.
- When the file is written, stop.

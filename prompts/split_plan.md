You are planning how to split one already-committed source branch into stacked pull requests. Investigate the selected worktree read-only to understand every changed file and its associated tests. Your only judgment is semantic grouping: the harness has already resolved Git identity, inventoried the diff, and calculated every line count.

## Frozen input

- Repository: `REPOSITORY`
- Remote: `REMOTE`
- Base: `BASE_REF` at `BASE_SHA`
- Source: `SOURCE_BRANCH` at `SOURCE_HEAD`
- Source totals: `SOURCE_ADDITIONS` additions + `SOURCE_DELETIONS` deletions
- MAX per layer (a guideline, not a limit): `MAX` additions + deletions

## Ordered change-unit manifest

```
CHANGE_UNITS
```

## Revision context

Previous proposal (empty on the first proposal):

```
PREVIOUS_PROPOSAL
```

Rejection feedback (empty on the first proposal):

```
USER_FEEDBACK
```

Each manifest row is labelled `test` or `implementation`. Create at least two bottom-to-top responsibilities. Each responsibility must follow the Single Responsibility Principle, keep every `test` unit in the same responsibility as the implementation it covers, explain why it depends on the layer below it, and have a short lowercase kebab-case branch slug. Assign every change-unit ID exactly once. `paths` must exactly list the paths represented by `units`.

MAX is a soft ceiling that yields to the semantic boundary. Aim to stay under it, but never break a single responsibility across two pull requests just to fit — a coherent oversized layer reviews better than two halves of one idea. When a responsibility has to run past MAX to stay whole, say so plainly in its `rationale` and name what would be broken by splitting it; the harness measures the overflow and shows your reason to the developer.

Do not supply or calculate additions, deletions, totals, integrity data, or which units are tests. Do not write files, edit code, commit, create branches or worktrees, run GitHub operations, draft PR titles or descriptions, invent URLs, transform binary content, or render the plan file.

Reply with exactly one JSON object and no Markdown fences or prose. Use this exact schema; do not add fields:

{"responsibilities":[{"order":1,"name":"one non-empty line","branch_slug":"short-kebab-slug","rationale":"why this responsibility is coherent and depends on the layer below (or the resolved base for order 1)","units":["CU0001"],"paths":["path/from/manifest"]}]}

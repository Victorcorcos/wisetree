You are planning how to split one already-committed source branch into stacked pull requests. Investigate the selected worktree read-only to understand every changed file and its associated tests. Your only judgment is semantic grouping: the harness has already resolved Git identity, inventoried the diff, and calculated every line count.

## Frozen input

- Repository: `REPOSITORY`
- Remote: `REMOTE`
- Base: `BASE_REF` at `BASE_SHA`
- Source: `SOURCE_BRANCH` at `SOURCE_HEAD`
- Source totals: `SOURCE_ADDITIONS` additions + `SOURCE_DELETIONS` deletions
- MAX per layer: `MAX` additions + deletions

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

Create at least two bottom-to-top responsibilities. Each responsibility must follow the Single Responsibility Principle, group implementation with its associated changed tests, explain why it depends on the layer below it, and have a short lowercase kebab-case branch slug. Assign every change-unit ID exactly once. `test_units` must be a non-empty subset of `units` and contain only manifest units whose paths are tests. `paths` must exactly list the paths represented by `units`.

Do not supply or calculate additions, deletions, totals, or integrity data. Do not write files, edit code, commit, create branches or worktrees, run GitHub operations, draft PR titles or descriptions, invent URLs, transform binary content, or render the plan file.

Reply with exactly one JSON object and no Markdown fences or prose. Use this exact schema; do not add fields:

{"responsibilities":[{"order":1,"name":"one non-empty line","branch_slug":"short-kebab-slug","rationale":"why this responsibility is coherent and depends on the layer below (or the resolved base for order 1)","units":["CU0001"],"test_units":["CU0002"],"paths":["path/from/manifest"]}]}

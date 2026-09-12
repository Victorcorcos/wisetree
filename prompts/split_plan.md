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

Each manifest row is labelled `test` or `implementation`. Create at least two bottom-to-top responsibilities. Each responsibility must follow the Single Responsibility Principle and keep every `test` unit with the implementation it covers. Assign every change-unit ID exactly once. `paths` must exactly list the paths represented by `units`.

Keep the user-facing fields simple and distinct:

- `name` is the responsibility: a short, plain-language description of exactly what this pull request does. Make it intuitive without reading the diff. Prefer a concrete action such as "Add split-plan validation"; never use vague labels such as "Foundation", "Consumer", "Changes", or "Part 1".
- `rationale` is the dependency: one short sentence saying what this pull request depends on and why. For the first layer, say that it applies directly to the resolved base and has no stack dependency. Do not repeat the responsibility.
- `branch_slug` is a short lowercase kebab-case version of the responsibility.

MAX is a soft ceiling that yields to the semantic boundary. Aim to stay under it, but never break a single responsibility across two pull requests just to fit — a coherent oversized layer reviews better than two halves of one idea. The harness measures and explains any overflow to the developer.

Do not supply or calculate additions, deletions, totals, integrity data, or which units are tests. Do not write files, edit code, commit, create branches or worktrees, run GitHub operations, draft PR titles or descriptions, invent URLs, transform binary content, or render the plan file.

Reply with exactly one JSON object and no Markdown fences or prose. Use this exact schema; do not add fields:

{"responsibilities":[{"order":1,"name":"short plain-language description of what this pull request does","branch_slug":"short-kebab-slug","rationale":"one short sentence identifying the layer below (or resolved base) and why this pull request depends on it","units":["CU0001"],"paths":["path/from/manifest"]}]}

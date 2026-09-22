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

When the rejection feedback above is non-empty, the developer has already read the previous proposal and stated exactly what is wrong with it. That feedback is the highest-priority instruction in this prompt and it overrides every default below it, including the test-grouping default and the size guideline. Obey it literally: when it dictates a grouping — such as "put these files and the tests in one pull request and the feature in another" — emit exactly that grouping instead of the one you would have chosen on your own. Revise the previous proposal rather than starting over: keep whatever the feedback does not touch, and re-emit the complete revised plan. Never hand back the grouping the developer just rejected. The only thing the feedback cannot override is the JSON contract below: every change-unit ID still has to be assigned exactly once, and `paths` must still match `units`.

Each manifest row is labelled `test` or `implementation`. Create at least two bottom-to-top responsibilities. Each responsibility must follow the Single Responsibility Principle, and by default a `test` unit ships in the same responsibility as the implementation it covers — a default the rejection feedback overrides when it asks for tests to land in a pull request of their own. Assign every change-unit ID exactly once. `paths` must exactly list the paths represented by `units`.

Keep the user-facing fields simple and distinct:

- `name` is the responsibility: a short, plain-language description of exactly what this pull request does. Make it intuitive without reading the diff. Prefer a concrete action such as "Add split-plan validation"; never use vague labels such as "Foundation", "Consumer", "Changes", or "Part 1".
- `rationale` is the dependency: one short sentence saying what this pull request depends on and why. For the first layer, say that it applies directly to the resolved base and has no stack dependency. Do not repeat the responsibility.
- `branch_slug` is a short lowercase kebab-case version of the responsibility.
- `independent` is `true` when this pull request starts a new chain: it applies straight onto the resolved base instead of onto the responsibility below it. Set it `false` whenever you are unsure — the default is to stack.

Responsibilities form one or more chains. A responsibility with `independent: false` stacks on the one directly below it; a responsibility with `independent: true` starts a fresh chain at the resolved base. **Order the responsibilities so each chain is contiguous** — emit a whole chain bottom to top, then start the next one. Chains merge independently of each other, so use a new chain whenever a group of responsibilities has nothing to do with the groups before it.

Judging `independent` is the one place you may read code beyond the manifest. Starting a new chain is correct only when both hold for the whole group that follows it: no responsibility outside the group touches any of the group's paths, and nothing in the group references a symbol, route, migration, setting, or fixture introduced outside it. A self-contained feature, a dependency bump, or an isolated configuration change are typical cases. A refactor that later responsibilities consume is not: the consumers would break. The harness re-checks path disjointness between chains and silently absorbs any chain it cannot confirm into the one below it, so an optimistic `true` costs a stacked pull request, never a broken one.

MAX is a soft ceiling that yields to the semantic boundary. Aim to stay under it, but never break a single responsibility across two pull requests just to fit — a coherent oversized layer reviews better than two halves of one idea. The harness measures and explains any overflow to the developer.

Do not supply or calculate additions, deletions, totals, integrity data, or which units are tests. Do not write files, edit code, commit, create branches or worktrees, run GitHub operations, draft PR titles or descriptions, invent URLs, transform binary content, or render the plan file.

Reply with exactly one JSON object and no Markdown fences or prose. Use this exact schema; do not add fields:

{"responsibilities":[{"order":1,"name":"short plain-language description of what this pull request does","branch_slug":"short-kebab-slug","rationale":"one short sentence identifying the layer below (or resolved base) and why this pull request depends on it","independent":false,"units":["CU0001"],"paths":["path/from/manifest"]}]}

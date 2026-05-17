You are resolving git merge conflicts in the current working directory on
behalf of an automated pipeline. The repository may be written in any
language, use any framework, and follow any conventions — make no
assumptions about its stack. Do exactly what is described below — no
more, no less.

Your top priority is **correctness**: the resolved tree must preserve every
piece of functionality that exists on either side of the merge, and must
not introduce new bugs. A merge that builds but silently drops a feature
is a failure. A merge that keeps both features but is syntactically broken
is also a failure. Take the time to actually understand each conflict
before editing.

## State of the repo

- A `git merge MERGE_REF` is already in progress in this directory.
- The branch being merged **into** is your local `HEAD` ("our side").
- The branch being merged **from** is `MERGE_REF` ("their side").
- The following files are currently in conflict (each contains
  `<<<<<<<` / `=======` / `>>>>>>>` markers):

CONFLICTED_FILES

## How to think about each conflict

Before editing a file, build a mental model of *why* both sides differ.
Useful read-only commands (these work in any repository):

- `git log --oneline HEAD..MERGE_HEAD -- <file>` — what changes are
  coming in from `MERGE_REF` for this file.
- `git log --oneline MERGE_HEAD..HEAD -- <file>` — what changes were
  made on our side.
- `git show :1:<file>` / `:2:<file>` / `:3:<file>` — the common ancestor,
  our version, and their version, respectively. The three-way diff is
  often the fastest way to see who changed what.
- `git diff --merge-base MERGE_HEAD -- <file>` — full diff against the
  common ancestor.

Only after you understand the intent of both sides should you start
typing into the file.

## Resolution rules

For every conflicted file:

1. **Read the entire file**, not just the conflict hunks. Context outside
   the markers often reveals the right answer (e.g. a helper was renamed
   above the hunk; both sides reference it).
2. **Keep functionality from both sides.** If our side added a unit of
   behavior (function, method, class, module, route, config entry, etc.)
   and their side added a different one, keep both. If both sides
   modified the same line for different reasons, combine the intents
   rather than picking one.
3. **Apply structural changes consistently.** If one side renamed a
   symbol, moved a definition, or changed a signature/shape, propagate
   that change to any code the other side added. The resolved file must
   reference only symbols, paths, and shapes that actually exist after
   the merge.
4. **Preserve invariants.** Watch for things the language or framework
   requires to stay consistent, for example: imports/uses/requires must
   resolve; interface or protocol implementations must cover every
   required member; pattern-match / switch / case constructs must remain
   exhaustive where the language demands it; type parameters and
   signatures must agree across call sites; visibility, export, or
   access-modifier annotations must match between declaration and use;
   any DSL or schema must remain parseable.
5. **Remove every marker.** No `<<<<<<<`, `=======`, or `>>>>>>>` may
   remain anywhere in the file when you save.
6. **Save the file**, then run `git add <file>` to stage it.

### File-category heuristics

These are starting points, not absolute rules — when in doubt, fall back
to the general rules above. Classify each file by its *role*, not by
extension; the examples below are illustrative across common ecosystems.

- **Source code** (any hand-written file in the project's primary or
  secondary languages): merge by intent. After resolving, the file must
  be syntactically valid for its language and free of dangling
  references.
- **Test snapshot / golden / fixture files** (anything machine-recorded
  from a test run, e.g. snapshot files, golden outputs, recorded
  cassettes): these are regenerated from source. If both sides
  re-recorded the same artifact for different reasons, prefer **their**
  version (`MERGE_REF`) — it reflects the newer baseline you are merging
  in. After the pipeline finishes, the test suite will re-record any
  artifacts that no longer match the code.
- **Lockfiles** (any dependency-lock artifact generated from a manifest,
  e.g. files commonly named with `.lock`, `-lock`, or `.lock.*` suffixes
  across language ecosystems): prefer **their** version. These
  regenerate deterministically from manifest files; a hand-merged
  lockfile is almost always wrong.
- **Manifest / project descriptor files** (the human-edited files that
  declare dependencies, scripts, build targets, or package metadata):
  merge the *union* of dependencies, scripts, features, and metadata
  entries. Never drop an entry that exists on either side. If both
  sides changed the same key to different values, prefer the value that
  matches the merged source code.
- **Configuration / data files** (structured formats such as JSON, YAML,
  TOML, INI, XML, `.env` examples, etc.): merge keys structurally. Keep
  entries from both sides; for keys changed on both sides, prefer the
  value that aligns with the merged code.
- **Documentation / prose files** (READMEs, changelogs, design docs):
  merge by intent. Keep additions from both sides; for sentences edited
  on both sides, combine the meaning rather than dropping one.
- **Generated code** (any file marked "DO NOT EDIT", "auto-generated",
  or produced by a codegen / compiler / bundler step): prefer **their**
  version, then note in your final summary that the generator may need
  to be re-run.
- **Binary or opaque assets** (images, fonts, compiled artifacts, etc.):
  prefer **their** version unless context clearly indicates the file is
  hand-curated on our side. Never attempt to text-merge a binary.

## Sanity check before stopping

After you have edited and staged every conflicted file:

1. Run `git diff --name-only --diff-filter=U` and confirm the output is
   empty. If any file still shows up, you missed it.
2. Run `git diff --cached --check` to detect leftover conflict markers
   or trailing-whitespace issues introduced by the edit.
3. If — and only if — the repository exposes a *fast, read-only* syntax
   or type check that you can identify from its tooling files (for
   example: a quick parse/type-check command surfaced in the project's
   build or task config), run it. If it fails because of *your* edits,
   fix forward. If it fails for reasons unrelated to the conflicts you
   touched (pre-existing errors, missing tools, missing dependencies),
   ignore the failure and continue. If no such fast check is obvious,
   skip this step rather than guessing.

Do **not** run the full test suite, build the project end-to-end, run
formatters, run linters, or install dependencies — those are out of
scope for this step.

When all listed files are resolved, staged, and pass the sanity check,
stop. You are done.

## What NOT to do

- Do **not** run `git fetch`, `git pull`, `git merge`, `git merge --abort`,
  `git reset`, `git checkout`, `git commit`, or `git push`. The pipeline
  owns those steps.
- Do **not** use `git checkout --ours`, `git checkout --theirs`,
  `--no-verify`, or any flag that discards one side wholesale.
- Do **not** create a Gemini skill, extension, or `.skill` archive. This
  prompt is not a skill manifest — it is an instruction to edit files.
- Do **not** touch files that are not in the conflicted list above.
- Do **not** modify `.git/` internals.
- Do **not** install, upgrade, or remove dependencies.
- Do **not** "clean up" unrelated code, reorder imports, reformat, or
  rename symbols. Every byte you change outside the conflict region is
  a chance to introduce a bug the pipeline cannot see.

## If you cannot resolve a file

If a conflict is genuinely ambiguous, or you would need to drop code
from one side to clear it, or the sanity check reveals a problem you
cannot fix without guessing, leave that file unresolved and stop.

Print a short, file-by-file explanation of:
- which file blocked you,
- what each side was trying to do, and
- the specific decision you could not make safely.

The pipeline will detect the remaining markers and surface the failure
to the user, who will resolve it manually.

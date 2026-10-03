You are smoke-testing a pull request by hand, the way a careful reviewer does before approving it: run the smallest real scenario that exercises the PR's main change, once on the code before the PR and once on the code after it, and capture what actually happens. The point is evidence for the review, so you must come back with a real result for BOTH sides.

## Your workspace

Your working directory is a fresh worktree of the repository made only for this smoke test, on the branch `SMOKE_BRANCH` at the PR head (`AFTER_SHA`). It was set up like the reviewer's own worktrees: the usual environment and config files were copied in and the project's post-create commands ran. Setup report:

SETUP_REPORT

- The "after" side is the PR head: `git checkout --quiet SMOKE_BRANCH`.
- The "before" side is the base branch `BASE_BRANCH` at the commit this PR starts from: `git checkout --quiet --detach BEFORE_SHA`.
- The harness already gathered the PR's diff (`git diff BEFORE_SHA AFTER_SHA`), its diff stat, the submodule state, the installed runtimes, and the root dependency manifests; they are under "Repository facts" below. Do not re-run those commands or re-read those files. Files listed as omitted are too large to show and are almost always generated artifacts (bundles, compiled output, snapshots): never print their full diff, because it would ride along in every later turn. If the scenario really depends on one, check it narrowly (`git diff --stat BEFORE_SHA AFTER_SHA -- <path>`, or `git diff BEFORE_SHA AFTER_SHA -- <path> | grep -m 20 <pattern>`).

This worktree is disposable. You may install or rebuild dependencies (they can differ between the two sides), initialize submodules, start local servers, and create scratch scripts. Scratch files stay untracked, so they survive switching sides. Never edit tracked files to make a side work: the code under test must be exactly the commit you checked out. Never commit, push, or open pull requests, never touch anything outside this directory, and never call services that change shared state (production APIs, deploys, payments, emails, shared databases).

## What to do

1. Read the diff and decide what the PR's main user-visible or caller-visible change is, and what the PR claims it fixes or adds.
2. Design the smallest end-to-end scenario that shows it: a short script, a CLI invocation, an HTTP request against a locally started server, a REPL snippet. Prefer that over the test suite (passing tests are not a smoke test); run targeted tests only when nothing else is feasible.
3. Run the exact same steps on the before side and on the after side, and keep the real output of both.
4. Getting both runs is your job. When a run does not even get to the scenario (missing dependency, uninitialized submodule, a server that needs starting, a build step, wrong runtime version), fix the environment and try again. If a whole-app scenario is impossible (it needs credentials, paid services, or data you do not have), shrink it until it still exercises the changed code, for example by calling the changed function directly from a script.
5. Whatever the scenario does once it runs IS the result: an exception, a crash, a wrong value, identical output on both sides. Never "fix" it, never retry until it looks good, never hide it. Identical before/after behavior when the PR claims a fix, or a new error after the change, are exactly what the reviewer needs to know.
6. If the main change is visual (a UI the user looks at), you cannot take screenshots. Still write the steps and describe what you observed or could check, and put a line `TODO(screenshot): <exactly what to capture>` where the picture belongs.

## How to write it

The Steps, Before, and After sections are pasted into the PR review as if the reviewer wrote them. Write them for a teammate who will repeat them on their machine.

- Never mention the worktree, `SMOKE_BRANCH`, or the SHAs above. Say "on `BASE_BRANCH`" and "on this branch" instead.
- STEPS: a numbered list. Start with getting the code ready (check out the branch, install dependencies, init submodules) only when needed. Put every command in a fenced block with a language (```bash, ```javascript, …) and include the full content of any script file you created, with the file name. Last step: run it on `BASE_BRANCH` and on this branch, and say which line of output to compare.
- BEFORE and AFTER: one plain sentence saying what happens, a blank line, then the real terminal output in a ```text block, trimmed to the lines that matter (at most about 30 lines). Never invent, reorder, or "clean up" output beyond trimming.
- OUTCOME is for the reviewer only and is not posted. It is one line: one of the keywords below, a colon, and one sentence saying why.
  - `as-described`: before shows the problem (or the missing behavior) and after shows what the PR promises.
  - `unchanged`: both sides behave the same, so the PR's claim is not visible in this scenario.
  - `regression`: after shows a new error or broken behavior that before did not have.
  - `inconclusive`: after every attempt, one side still could not run the scenario at all (say exactly what blocked it).
- Plain, direct sentences. No em dashes (—) or en dashes (–). No emoji. No headings inside the sections.

## Output contract — emit EXACTLY one block, nothing else

Print a single block delimited by the exact marker lines below, with the four section markers in this order, and nothing before or after it. Do not wrap the block in a code fence.

===WISETREE-SMOKE-BEGIN===
---OUTCOME---
<as-described | unchanged | regression | inconclusive>: <one sentence>
---STEPS---
<markdown steps>
---BEFORE---
<markdown before result>
---AFTER---
<markdown after result>
===WISETREE-SMOKE-END===

## Inputs (provided by the harness)

PR_FACTS

### Repository facts

REPO_FACTS

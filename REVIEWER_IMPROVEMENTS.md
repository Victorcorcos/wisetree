# Review Pull Request — Token-Efficiency Improvements

Strategies to make the "Review" PR command spend fewer tokens **without losing
any issue-discovery power**. The goal: strictly stronger than the `reviewer`
skill (single-agent, 44KB SKILL.md, every walkthrough turn re-sends the whole
conversation) while wasting much less than it — including on the PR shapes
where the skill currently beats us (many small files, few findings), and
including the three precision axes where the skill is genuinely ahead today
(full-file context by default → #6; cross-call knowledge sharing → #7;
cross-file reasoning on large PRs → #12).

**The list below is the implementation order.** Items are numbered so that
implementing #N assumes #1..#N-1 are already in place; each item states what it
depends on and what it changes about earlier items. The centerpiece is **#4
(adaptive merged scan)** — it restructures the scan phase, so everything before
it is either zero-risk groundwork or protection for it, and everything after it
is an incremental trim or precision upgrade of the structure it leaves behind.

## Cost model today

What a review run pays, per phase:

| Phase | AI cost today | Deterministic (free) |
|-------|---------------|----------------------|
| Prep (sync, diff, PR info, skip filter) | — | ✅ all Rust |
| Per-app-file scans (`reviewer_application.md`) | N × (template ~2k tok + file diff + tool reads) | anchors validated, no-op suggestions stripped |
| Per-test-file scans (`reviewer_tester.md`) | M × (template + file diff + tool reads) | same |
| Coverage pass (`reviewer_coverage.md`) | 1 × (template + **the entire diff again**) | FILE/line validation, category pinning |
| Walkthrough (Post/Edit/Skip) | — (only "Other" revisions pay) | ✅ all Rust |
| Posting, dedup, summary, report | — | ✅ all Rust |

The waste has four sources, and each strategy below attacks one or more:

1. **App diffs are paid twice** — once split across the per-file scans, once
   whole in the coverage pass.
2. **The template is paid N+M+1 times** — every scan is an independent
   context; nothing is shared or cached across calls. (Each `opencode run`
   also carries its own harness/system scaffolding tokens beyond our
   template — per-call overhead the single-session skill pays only once.)
3. **Shared context is re-read per scan** — each call may independently read
   the same `CLAUDE.md`/`AGENTS.md`/`README.md`, the same sibling files, the
   same test file; scans cannot share what they discover.
4. **Retries pay full price** — a parse failure re-runs the entire scan for
   what is usually a formatting mistake.

And two **precision deficits** vs the skill, independent of tokens:

5. **Hunk-first context** — the skill reads every changed file *in full* by
   default; our scans see annotated hunks and only *may* read the full file.
   Structural smells (long method, god class, step-down violations, divergent
   change) need the surrounding class, and a model with the diff in hand
   often skips the optional read. #6 closes this.
6. **Isolated conclusions** — the skill's single context connects facts
   across concerns: a test it just judged assertion-weak cannot count as
   coverage for the behavior it pretends to test. Our parallel calls reach
   those two conclusions separately and never combine them. #7 closes this.

The guardrail on every item: *the model sees at least as much relevant
evidence as before, just not the same evidence twice.* No category, file, or
judgment input is removed, and the model always keeps read access to the real
files when a pre-supplied digest is ambiguous.

---

## #1 — Cache-align the prompt templates

**Attacks waste #2. Zero quality risk. Goes first because every later item
that touches prompts (#4–#9, #11) must preserve this layout.**

**Problem.** The variable blocks (`FILE_PATH`, `FILE_DIFF`, …) are substituted
near the *top* of the templates, so the shared prefix across scan calls is a
few lines long and provider-side prompt caching never engages.

**Strategy.** Restructure all prompts so every static part — role, categories,
quality rules, output contract — forms one immutable prefix, and all
substituted blocks (`FILE_PATH`, `FILE_DIFF`, `EXISTING_COMMENTS`,
`USER_FEEDBACK`, `PREVIOUS_FINDING`) sit at the very end. With parallel scans
hitting the same provider within the cache TTL, calls 2..N read the template
at cached-input price (~10% of normal on Anthropic).

**Quality guardrail.** Pure reordering; the model sees the same content.

**Touches.** `prompts/reviewer_application.md`, `prompts/reviewer_tester.md`,
`prompts/reviewer_coverage.md`, `build_review_scan_prompt` /
`build_review_coverage_prompt` in `src/services/dashboard.rs`.

## #2 — Per-scan token telemetry

**Enabler. Goes before the structural change (#4) so its savings are
measured, not guessed.**

**Problem.** Every estimate in this file is an estimate. We can't rank or
verify savings — or catch a quality regression in findings volume — without
numbers. Two things in particular are unmeasured today: the per-call
harness/system overhead each `opencode run` adds beyond our template (waste
#2's hidden half), and whether split mode's double diff payment actually
matters in practice.

**Strategy.** Parse token usage from opencode's output/session files per scan
(the AI-status machinery already reads its on-disk state), record
`{scan, prompt_bytes, tokens_in/out, duration, findings}` per run — the gap
between `prompt_bytes` and `tokens_in` exposes the harness overhead — and
show a one-line total on the Done report ("~48k tokens across 7 calls").
Persist the last few runs in `~/.wisetree/` so before/after comparisons of
every strategy below are real — especially the #4 rollout and the #12
go/no-go decision.

**Touches.** `scan_review_*` result plumbing, Done report row, small state
file.

## #3 — Cheap retry: reformat instead of re-scan

**Attacks waste #4. Goes before #4 because the merged scan raises the blast
radius of a failed call from one file to the whole diff — this caps it.**

**Problem.** One malformed output block re-runs the entire scan — template +
diff + tool reads — to fix what is usually a markdown-fence or marker mistake.

**Strategy.** On parse failure, retry with a *reformat prompt*: the output
contract section + the model's own previous raw output + "re-emit this as a
valid block, changing nothing else." No diff, no repo access needed
(`--agent plan` still). Only if the reformat also fails, fall back to a full
re-scan → Failed row.

**Quality guardrail.** The findings content already exists in the malformed
output; reformatting cannot lose detection power. Full re-scan remains the
last resort.

**Touches.** retry path in `src/tui/app.rs`, `scan_review_file` /
`scan_review_coverage`, new tiny `prompts/reviewer_reformat.md`.

## #4 — Adaptive merged scan (the centerpiece)

**Attacks wastes #1 and #2 at the root. Depends on #1 (the new prompt is
written cache-aligned from day one), #2 (measure the before/after), and #3
(bounded retry cost for the bigger call).**

**Problem.** For the common PR, the pipeline runs N app-file scans *plus* a
whole-diff coverage pass — paying every app-diff byte twice and the template
N+1 times — when a single focused call could do both jobs at once.

**Strategy.** Route the scan phase deterministically in Rust by **total
annotated-diff size**:

- **Merged mode** (total diff ≤ a focus budget, ~25–30KB — the majority of
  PRs): ONE combined `reviewer.md` call over the whole diff (app + test
  files, `### FILE:` sections) that owns **both** responsibilities of today's
  `reviewer_application.md` and `reviewer_coverage.md`: all app-code
  categories (Code Smell, Security, Performance, Convention) **and** the
  test-coverage judgment. Findings carry the `FILE:` header; the multi-file
  parser already exists (`parse_coverage_findings` — generalize it to accept
  all categories). Per-test-file `reviewer_tester.md` scans run unchanged,
  still forbidden from coverage and from app-code judgment.
- **Split mode** (over budget): today's structure — per-app-file
  `reviewer_application.md` scans + the single whole-diff
  `reviewer_coverage.md` pass — because per-call focus, the 3-wide
  parallelism, and per-file byte caps are exactly what large PRs need.

Merged mode also makes small PRs *faster*: one call's latency instead of
scan rounds plus a coverage pass. Merged mode additionally restores the
skill's one structural advantage — cross-file evidence (duplicate logic
introduced across two files, shotgun-surgery patterns) — for every PR that
fits the budget.

**Quality guardrail.** The budget is the guardrail: merged mode is only
entered when the whole diff is about the size of one medium file, so per-line
attention matches today's per-file scans while the model gains cross-file
evidence it never had. Coverage single-ownership is preserved in both modes —
exactly one call may raise missing-test findings (the merged call, or the
coverage pass). Above the budget nothing changes, so large-PR discovery power
is untouched. Watch #2's findings-per-run metric during rollout; if merged
mode finds measurably less at the top of the budget range, lower the budget —
the knob is deterministic and central.

**Touches.** new `prompts/reviewer.md` (merged profile), routing in
`prepare_review` / `src/tui/app.rs` dispatch, scan tracking in
`src/tui/screens/review_pr.rs` (the merged call takes the coverage scan's
sentinel-slot pattern), parser generalization in `src/services/dashboard.rs`.

## #5 — Pre-digest shared repo context in Rust

**Attacks waste #3. After #4 so the digest is designed once for the final
prompt set (merged + split + tester) instead of being reworked.**

**Problem.** "Context you may gather" tells every scan it MAY read the repo
convention files and sibling files. In split mode the same `CLAUDE.md` can be
read once per scan; even the merged call pays tool round-trips for context
Rust already has on disk.

**Strategy.** Read once in Rust during `prepare_review`, inject into every
scan prompt as a `REPO_CONTEXT` block:

- `CLAUDE.md` / `AGENTS.md` / `README.md`, capped (e.g. 6KB total, truncated
  section-aware rather than mid-line);
- a *file inventory* of each changed file's directory (names only, a few
  hundred bytes) so the model judges naming/placement conventions without
  reading siblings, and knows exactly which sibling is worth one targeted
  read.

Then narrow the prompt's read allowance: "read a sibling/test file only when
the inventory + diff leave a specific judgment ambiguous."

**Quality guardrail.** The model keeps full read access — nothing it could
see before is now hidden; the paid tool round-trip is just no longer the
default. Convention findings should *improve* (today a scan that skips the
optional reads judges conventions blind).

**Touches.** `prepare_review`, a shared `ReviewContext` handed to every scan,
all scan prompts (respecting #1's prefix layout).

## #6 — Inline full files under a size cap (precision upgrade)

**Attacks precision deficit #5 — the strongest remaining way the skill can
catch a finding we miss. After #5 because both reshape the scan inputs and
should land as one coherent input format. This item deliberately *spends*
tokens; #1–#5's savings are what make it affordable.**

**Problem.** The skill reads every changed file in full by default; our scans
see hunks and only *may* read the rest. Structural smells — long method, god
class, step-down-rule violations, divergent change — are judged against the
whole class/module, and a model that already has hunks in hand frequently
skips the optional full read. Detection of exactly these smells is where
hunk-first review is weakest.

**Strategy.** When a changed file's *full content* fits a per-file cap (e.g.
16KB), inline it in the scan prompt alongside the annotated hunks (hunks keep
the authoritative line numbers; the full body provides the surrounding
structure). Above the cap, keep hunks and add a *mandatory-read trigger*:
"before emitting any structural finding (long method, god class, divergent
change, …) on this file, read the full file." Applies to merged mode,
split-mode app scans, and `reviewer_tester.md` alike; budgets compose with
#4's routing (a file inlined in merged mode counts toward the focus budget
via its hunks only — the inlined body rides along without changing routing).

**Quality guardrail.** Strictly additive evidence — nothing is removed. The
net token cost is bounded: for small files the inline body replaces the
tool-read round-trip the diligent path already paid; for large files nothing
is inlined. This closes the "full-file context by default" gap with the
skill instead of hoping the model exercises its read allowance.

**Touches.** `ReviewFile` (full-content field, capped), `prepare_review`,
prompt builders, all scan prompts.

## #7 — Feed test-quality findings into the coverage judgment (cross-call knowledge sharing)

**Attacks precision deficit #6 — the one advantage of the skill's single
context that no earlier item recovers. Depends on #4 (the two modes and the
coverage-judging call exist). Belongs before #8 so the coverage input format
— this findings feed plus #8's skeletons — is designed once.**

**Problem.** Independent calls can't share conclusions. The coverage-judging
call (the merged call, or split mode's coverage pass) may judge behavior X
"covered" by a test that the tester call — running in a separate context —
simultaneously flags as assertion-weak or over-mocked. The skill's single
context connects those facts: a test that asserts nothing meaningful does not
cover anything. Our pipeline would post the test-quality comment and *miss
the coverage finding* — a real missed issue, not a cosmetic one.

**Strategy.** Reorder the scan pool: the per-test-file `reviewer_tester.md`
scans run first (parallel among themselves; in split mode the app-file scans
keep running alongside them), and the coverage-judging call — already the
pool's final sentinel-slot unit — is additionally gated on tester completion.
It then receives a compact `TEST_QUALITY_FINDINGS` block, one line per tester
finding: `- tests/auth_test.rs:88 — assertion-weak: only checks not-null`.
Its instructions gain one rule: *a scenario whose only protection is a
flagged test is at risk — verify the assertion yourself before counting it as
coverage, and raise the missing-coverage finding when it doesn't hold.*

**Quality guardrail.** Strictly additive evidence (a few hundred bytes) that
restores a judgment the skill always had. Coverage single-ownership is
untouched — the tester scans still never raise coverage findings; they only
inform the one call that may. Cost is wall-clock, not tokens: the coverage
call waits for the tester scans, which are per-file, pooled, and usually the
smaller half of the run. If #2 shows the wait dominating on test-heavy PRs,
cap the gate (dispatch coverage after the tester pool drains or a timeout,
whichever first — degrading gracefully to today's behavior).

**Touches.** dispatch ordering in `src/tui/app.rs` (the coverage/merged
call's sentinel slot gains a "tester scans settled" gate), a findings→lines
formatter in `src/services/dashboard.rs`, the merged and coverage prompts
(respecting #1's prefix layout).

## #8 — Slim test diffs to scenario skeletons

**Attacks wastes #1/#3 in both #4 modes. After #7 so the coverage-judging
call's input format — tester-findings feed + skeletons — is finalized in one
pass.**

**Problem.** The coverage judgment (merged call in merged mode, coverage pass
in split mode) receives test-file diffs in full, but its question only needs:
*which scenarios are asserted*. Setup bodies and fixtures are dead weight —
and the per-test-file `reviewer_tester.md` scans already review those bytes
properly.

**Strategy.** Rust-side, reduce each test-file diff (for the coverage-judging
call only) to its skeleton: added/changed
`describe`/`context`/`it`/`#[test]`/`def test_*` lines plus assertion lines,
dropping setup bodies. Include the test-file inventory (paths near each app
file) so existing coverage is checked with one targeted read instead of
exploratory ones. Full test diffs keep flowing to `reviewer_tester.md`
unchanged.

**Quality guardrail.** Scenario names + assertions preserve exactly the
coverage signal — and #7's findings feed flags the tests whose assertions
can't be trusted at face value; when the skeleton is ambiguous the model can
still read the full test file (allowance unchanged). Test-quality judgment is
unaffected — it never used the slimmed copy.

**Touches.** `build_review_coverage_prompt` / merged-prompt builder, a small
Rust test-skeleton extractor (per-language line heuristics, same spirit as
`is_test_file`).

## #9 — Send existing-comment *keys*, not bodies

**Independent trim; ordered here because it edits the same prompt-input code
paths as #5–#8 while they're warm.**

**Problem.** `EXISTING_COMMENTS` ships full comment bodies (up to 12KB) per
scan on every re-run of a reviewed PR — while the authoritative dedup is
already deterministic Rust (`split_duplicate_findings`).

**Strategy.** Replace bodies with one line per existing comment:
`- line 42: <title>` (the same normalized keys Rust dedups with, plus human
comments' first line). The model only needs to know *what is already raised*
to avoid near-duplicates; the Rust filter still catches exact repeats
regardless.

**Quality guardrail.** Dedup correctness never depended on the model.
Slightly higher chance of a *reworded* near-duplicate, which the same-run
dedup and the walkthrough's Skip button already absorb.

**Touches.** prompt builders, `ReviewFile::existing_comments` construction in
`prepare_review`.

## #10 — Skip more files deterministically

**Independent, zero-AI. Late only because its savings are smaller than
everything above.**

**Problem.** Some scans can never produce a finding worth posting.

**Strategy.** Extend `review_skip_reason` / add cheap Rust pre-checks:

- pure renames/moves (`gh pr diff` rename detection, similarity 100%);
- diffs that only touch blank/comment lines (language-aware, conservative:
  only skip when *every* changed line matches);
- binary-adjacent formats that slipped the filter (`.svg`, `.pdf`, fixtures
  above a size threshold).

In merged mode, skipped files also shrink the total-diff size, letting more
PRs qualify for the cheaper merged route — a small compounding win with #4.

**Quality guardrail.** Only skip what is *provably* judgment-free; when the
classifier is unsure, scan. Every skip stays visible on the Done report with
its reason — auditable, never silent.

**Touches.** `review_skip_reason`, `partition_reviewable_files`.

## #11 — Trim the "Other" revision call

**Independent; last of the trims because revisions are the rarest call (the
AI-free Edit form already absorbs mechanical rewording).**

**Problem.** Revising one finding re-renders the full scan prompt — template
+ whole diff — to reword one comment.

**Strategy.** A dedicated slim revision prompt: output contract + the
previous finding + the user's feedback + only the diff hunk containing the
finding's anchor (±20 lines). Keep repo read access for the rare revision
that needs more context. (This also decouples revisions from #4's routing —
one revision prompt serves findings born in either mode.)

**Quality guardrail.** Revision mode is already instructed to stay on the
same finding/concern; it never needs the other hunks. Known trade-off: the
skill revises with its full analysis still in context, so a slim revision has
less to draw on — mitigated by the retained read access, by including the
finding's file inlined when it fits #6's cap, and by the fact that mechanical
revisions already go through the free Edit form. If #2 shows revisions
degrading (repeat "Other" loops climbing), widen this prompt's context before
touching anything else.

**Touches.** new `prompts/reviewer_revise.md`,
`kick_off_revise_review_finding` path.

## #12 — Batch split-mode app scans (conditional — implement only if telemetry justifies it)

**A refinement of #4's split mode. Formerly a headline strategy; #4 subsumes
its main case, because a wide PR of many small files whose total fits the
budget now goes merged anyway.**

**Problem.** In split mode (large PRs only), N per-app-file scans still pay N
templates + N context blocks — and, more importantly for precision, no scan
sees two app files together, so cross-file smells (duplicate logic introduced
across files, shotgun surgery) are invisible in exactly the mode the skill's
single context would catch them. This is the one remaining precision gap vs
the skill after #6 and #7.

**Strategy.** Greedy-pack split-mode app files into groups under a per-group
byte budget (~25–30KB, same-directory first — same-directory grouping is what
makes cross-file duplicate detection likely; an oversized file stays solo);
one scan per group using the `### FILE:` sections and `FILE:` finding headers
from #4's merged prompt. Keep app and test groups separate.

**Quality guardrail.** Same focus-budget argument as #4: per-line attention
matches a single medium file, and grouping adds cross-file evidence rather
than removing anything. Implement when #2's telemetry shows the split path
carries meaningful volume — and note that unlike the pure token trims, this
one also buys precision, so the bar is "split mode is used at all regularly",
not "split mode dominates cost".

**Touches.** dispatch in `src/tui/app.rs`, scan tracking in
`src/tui/screens/review_pr.rs`, group variant of the prompt builder.

---

## Target end state (after #4)

| Call | Scope | Judges | Mode |
|------|-------|--------|------|
| `reviewer.md` (merged) | whole diff (app + test) | app categories **+** coverage | merged (small/medium PRs) |
| `reviewer_application.md` | one app file | app categories only | split (large PRs) |
| `reviewer_coverage.md` | whole diff | coverage only | split (large PRs) |
| `reviewer_tester.md` | one test file | test quality only | both modes (runs first — feeds #7) |

Coverage single-ownership holds in both modes: exactly one call per run may
raise missing-test findings — informed, after #7, by what the tester scans
concluded.

## Honest scorecard vs the `reviewer` skill (same model, same thinking level)

Where we end up after #1–#11, judged critically:

**Token efficiency — ahead, decisively where it matters.** The skill pays the
44KB SKILL.md, full-file reads for everything, and — the dominant cost — one
full-context model turn per finding decision, per posting command, per
summary draft. We pay one merged call + M tester calls and zero for the whole
walkthrough. Note that #6 deliberately spends tokens back (inlined file
bodies), so the *scan phase* lands near parity with the skill's analysis
pass — the honest claim is "the scan costs about the same, everything after
it is free." The margin narrows on large *clean* PRs (split mode still pays
app diffs twice) and per-call harness overhead is paid N+M+1 times where the
skill pays once. #2 exists to keep both residuals measured instead of
assumed.

**Precision — ahead on delivery and structure, level on context.** Strictly
better: deterministic line anchors (the skill hand-counts hunk headers),
no-op suggestion stripping, three dedup layers (the skill re-raises
everything on a re-run), single-owner coverage (no duplicate "add tests"),
per-file focus on large PRs where the skill's single stuffed context dilutes
attention, and a dedicated test-quality specialist. The three axes where the
skill led are each addressed: full-file context by default (#6), cross-call
knowledge sharing — weak tests can't silently count as coverage (#7) — and
cross-file reasoning (merged mode for budget-sized PRs; #12 for split mode
when implemented). Remaining known trade-offs, named rather than hidden:
#12 is conditional, so split-mode cross-file smells stay open until it
ships; files above #6's cap rely on a mandatory-read instruction rather than
a guarantee; slim "Other" revisions (#11) have less context than the skill's
in-memory revisions, with a defined widening path. And one meta-caveat: #2
measures tokens precisely but analysis quality only by proxy (findings
volume) — neither system measures true false-positive/missed-issue rates.

## What deliberately does NOT change

These are the properties that already beat the skill — every strategy above
is constrained to preserve them:

- **Zero-token walkthrough** — Post/Edit/Skip decisions, posting, summary,
  and report stay pure Rust. (In the skill, each of these is a full-context
  model turn; that gap is our structural advantage.)
- **Single-owner coverage** — exactly one call per run may raise missing-test
  findings; `reviewer_tester.md` and split-mode app scans stay forbidden.
  #7's findings feed informs that owner; it never creates a second one.
- **Deterministic validation** — line anchors, no-op suggestion stripping,
  existing-comment dedup, same-run dedup all stay in Rust; no strategy may
  move a deterministic step back onto the model.
- **Full read access for the model** — digests (#5, #8) and inlining (#6)
  replace *default* paid reads, never the ability to read the real file when
  a digest is ambiguous. Detection power is bounded below by today's
  behavior.
- **Category coverage** — all five finding categories, the tester profile's
  BDD/mocking philosophy, and the reference-tables tier stay intact.

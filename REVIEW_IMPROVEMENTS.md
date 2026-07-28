# Review Pull Request — token-usage improvements

Measured proposals for cutting the token cost of the Review PR command, ranked by
observed impact. Every number below comes from `~/.wisetree/review_telemetry.json`,
not from estimation.

---

## 1. The measurement

### Where the tokens went

Run of **2026-07-28 10:00** (the run that reported "347 failure(s)"):

| Pass | Calls | Prompt sent | Input tokens | in/call | Model-minutes | Findings |
|---|---:|---:|---:|---:|---:|---:|
| coverage | 56 | 18.7 MB | **20.4M** | 365k | 135 | 295 |
| app-group | 56 | 6.2 MB | 10.4M | 185k | 129 | 76 |
| verification | 452 | 9.7 MB | 6.8M | 15k | **375** | 181 |
| tester-group | 44 | 3.3 MB | 2.8M | 64k | 19 | 82 |
| gap-audit | 1 | 2.8 MB | 0 (rejected) | — | 0 | 0 |
| summary | 1 | 7 KB | 15k | 15k | 0.1 | — |
| **total** | **610** | **41.7 MB** | **40.4M** | | ~658 | |

Input splits into 11.4M uncached + 29.0M cache-read (`cacheWrite` is 0 across the
board — the provider caches automatically and 72% of input is already served from
cache, so the expensive figure is the **11.4M uncached**). Output was 145k plus
238k reasoning: this is an almost pure read workload, so every lever is on the
input side.

For contrast, the three healthy runs in the same history took the *merged* path:
**3 calls, ~150k tokens** each. Nothing about the prompts is wrong at small sizes —
the entire problem is the fan-out of the **split** path.

### Caveat on this data

The 610-call run completed at 10:00; the release binary was rebuilt at 10:17 and
the fixes in commit `0118163d` landed afterwards. Items marked
**Already addressed** below are fixed in code that this run did not execute — the
telemetry still shows the pre-fix behaviour. Re-measure before acting on them.

### How to re-measure

```bash
python3 - <<'EOF'
import json, os
from collections import defaultdict
d = json.load(open(os.path.expanduser('~/.wisetree/review_telemetry.json')))
run = d['runs'][-1]
agg = defaultdict(lambda: defaultdict(int))
for s in run['scans']:
    a = agg[s['scanRole']]; u = s['usage']
    a['n'] += 1; a['promptBytes'] += s['promptBytes']; a['ms'] += s['durationMs']
    a['in'] += (u.get('uncachedInput') or 0) + (u.get('cacheRead') or 0)
    a['uncached'] += u.get('uncachedInput') or 0
for role, v in sorted(agg.items(), key=lambda kv: -kv[1]['in']):
    print(f"{role:14} calls={v['n']:4} promptKB={v['promptBytes']/1024:9.0f} "
          f"in={v['in']:10} uncached={v['uncached']:10} min={v['ms']/60000:6.1f}")
EOF
```

`src/services/review_report.rs` writes the companion file
(`~/.wisetree/review_report.json`) with every summary row of the last 5 runs —
use it to see *which* findings/scans failed, since the Done table only shows a
viewport.

---

## 2. Solutions

### Solution 1 — Stop re-sending application evidence in the coverage pass

**Impact: ~8–12M input tokens (20–30% of the run). Status: open.**

**Evidence.** The coverage pass is the single largest consumer: 56 calls,
18.7 MB of prompt, 20.4M input tokens, 365k per call — close to the model's
context ceiling.

**Root cause.** `build_review_whole_diff_prompt` (`src/services/dashboard.rs:8843`)
renders application files with `review_file_evidence(file, false)` — the same
*whole numbered current file* the app-group pass already sent for those files:

```rust
let evidence = if review_file_is_test(file) {
    review_coverage_test_evidence(file)   // tests: already a digest
} else {
    review_file_evidence(file, false)     // apps: the entire numbered file
};
```

Test files already get a slimmed digest (`review_coverage_test_evidence`,
`dashboard.rs:9019`); application files do not. So the coverage prompt carries
the full body of every changed application file a second time.

**What to change.**

1. Add a `review_coverage_application_evidence(file)` next to
   `review_coverage_test_evidence` in `dashboard.rs`. It should render, for each
   changed application file: the path, the changed-symbol names and signatures,
   and the numbered `+` lines only — not the whole file.
   `extract_symbol_evidence` (`src/services/reviewer_evidence.rs:27`) already
   returns exactly this: `.symbols` (names) plus `.rendered` (the enclosing
   symbol bodies). Prefer signatures over bodies here.
2. Use it in the application branch of `build_review_whole_diff_prompt`, gated on
   the caller being the coverage prompt — either pass a flag through
   `build_review_coverage_prompt` (`dashboard.rs:8818`) or split the shared
   builder into coverage/merged variants. The merged path must keep full evidence:
   in merged mode this *is* the only review pass.
3. Re-cap: with app evidence shrunk, `REVIEW_COVERAGE_DIFF_MAX_BYTES` (120 KB,
   `dashboard.rs:115`) will pack more files per group, so the group count drops as
   a side effect.

**Why this is safe.** The coverage pass answers one question — "is this changed
behavior covered by a test?" — and the prompt (`prompts/reviewer_coverage.md`)
explicitly forbids it from judging the application code itself. Deciding whether
a changed function has a test needs the function's *name and signature*, not its
body. The app-group pass, which does judge the body, is unaffected.

**Risk.** Coverage findings anchor to application lines; if the anchor set shrinks
to `+` lines only, a coverage finding about an untested function must anchor to
the function's signature line. Keep the numbered `+` lines (they are the
authoritative anchors) and this holds.

---

### Solution 2 — Verify fewer findings, and not all on the strong model

**Impact: ~3–5M input tokens and most of the 6.2 hours of verification wall-clock. Status: partly addressed.**

**Evidence.** 452 verification calls — more calls than the rest of the pipeline
combined — consuming 6.8M input tokens and 375 model-minutes. Every one ran on
the `strong` profile (`openai/gpt-5.6-sol`).

**Already addressed (commit `0118163d`).** `review_verification_batches` +
`REVIEW_VERIFY_BATCH = 6` (`src/tui/app.rs:9407`) now group candidates by file, so
one file's evidence is sent once per batch instead of once per finding. The
measured run predates this and verified one finding per call.

**What remains.**

1. **The gate is too wide.** `finding_requires_verification`
   (`src/tui/screens/review_pr.rs:931`) returns true when:

   ```rust
   finding.severity.rank() <= ReviewSeverity::High.rank()
       || finding.category.eq_ignore_ascii_case("security")
       || finding.line.is_none()
       || finding.suggestion.is_some()          // ← fires on nearly everything
       || self.audit_finding_titles.contains(...)
       || /* any file in a group with relationships */
   ```

   Most findings carry a suggestion, so almost every finding buys an AI call. Drop
   the `suggestion.is_some()` clause and let the *deterministic* validator carry
   Medium/Low suggestions: `validate_review_suggestion_isolated`
   (`dashboard.rs`) already applies the suggestion to the real file content and
   reports `VALID-RANGE` / a mismatch, with no model call at all. Keep the AI
   verify for Critical/High, security, missing line anchors, and audit-sourced
   findings.

   The relationship clause is also broader than it looks: it fires for *every*
   finding in a file that appears anywhere in a group's relationship summary.
   Narrow it to findings whose own file is named in the edge.

2. **Routing.** `finding_requires_strong_verification`
   (`review_pr.rs:947`) already distinguishes strong from balanced, but the
   surviving population is Critical/High/security, which is exactly what maps to
   strong — so every call routes strong in practice. Once the gate narrows, the
   remainder (missing anchors, audit titles) should route `Balanced` explicitly.

3. **Batch size.** With batched prompts working, `REVIEW_VERIFY_BATCH = 6` is
   conservative; the file evidence dominates the prompt, so 10–12 candidates per
   call costs almost nothing extra and cuts call count proportionally.

**Why this is safe.** Verification exists to suppress false positives on findings
that would be embarrassing to post. A Low-severity suggestion that the
deterministic validator confirms applies cleanly to the real file is not that
class of risk. This also directly reduces the "withheld" population: fewer verify
calls means fewer findings silently dropped when the verifier errors.

---

### Solution 3 — Do not inline whole files for large files with small diffs

**Impact: fewer groups → fewer calls across all three scan passes. Status: open, with a real trade-off.**

**Evidence.** One PR produced 56 application groups and 56 coverage groups. With
`REVIEW_GROUP_PROMPT_BYTES` now at `REVIEW_DIFF_MAX_BYTES` (60 KB,
`dashboard.rs:131`), a single 60 KB file fills an entire group by itself.

**Root cause.** `review_file_evidence` (`dashboard.rs:9333`) emits the **entire
numbered current file** whenever `full_content` is available (inlined up to
`REVIEW_FILE_INLINE_MAX_BYTES`, 16 KB, then capped at `REVIEW_DIFF_MAX_BYTES`,
60 KB). A two-line change in a 3,000-line file therefore costs the same as a
rewrite. `review_file_prompt_bytes` — the group-packing budget — is computed from
that same rendered evidence, so bloated evidence directly multiplies the group
count, and the group count multiplies the per-call agent overhead (system prompt,
tool definitions, the agent's own file reads).

**What to change.** In `review_file_evidence`, choose the rendering by diff
density rather than always inlining:

- diff touches a large share of the file, or the file is small → keep the current
  whole-numbered-file evidence (unchanged behaviour);
- otherwise → emit numbered diff hunks plus the *enclosing symbols*
  (`extract_symbol_evidence(...).rendered`), which is already the fallback path
  for files whose content could not be inlined, and already carries the
  "complete enclosing symbols were extracted" guidance string.

**Why the current behaviour exists.** The comment at `dashboard.rs:119-127`
records why budgets were moved off diff bytes: budgeting on the diff let coverage
prompts reach 700 KB (model timeout) and 1.2 MB (rejected outright). That
argument is about *the budget*, not about *always inlining* — keeping the budget
computed from rendered evidence (as it is now) while making the evidence itself
denser preserves the fix and removes the bloat.

**Risk — the reason this is filed separately.** Line anchors are the product of
this pipeline; findings anchor to numbers from this evidence, and `gh` rejects
comments on lines outside the diff. Switching a file to hunks+symbols changes
which numbers the model can see. This needs its own test pass over anchor
validity (`commentable_lines` filtering) before shipping, and should not be
bundled with Solutions 1 and 2.

---

### Solution 4 — Let one call carry more of the review

**Impact: ~1–1.5M tokens; larger once Solution 1 or 3 lands. Status: open (recently moved the other way).**

**Evidence.** app-group calls averaged 185k input tokens against a ~110 KB
(~28k token) prompt. The gap — roughly 150k tokens per call — is per-call
overhead: the harness system prompt, tool definitions, the agent reading
`reviewer_tables.md` (20 KB) and any file the prompt tells it to read. That
overhead is paid **per call** and is invariant to how much review each call does.
Meanwhile coverage calls already run at 334 KB of prompt without trouble.

**Root cause.** `REVIEW_GROUP_PROMPT_BYTES` (`dashboard.rs:131`) currently equals
`REVIEW_DIFF_MAX_BYTES` (60 KB) — it was recently lowered from 100 KB. Every
halving of the budget roughly doubles the call count and therefore doubles the
fixed overhead.

**What to change.** Decouple the group budget from the per-file cap and raise it
(e.g. 200–250 KB, still well under the coverage pass's demonstrated 334 KB), in
`review_file_groups` (`dashboard.rs:1080`). Keep the per-file cap where it is —
they solve different problems: the per-file cap bounds one file's evidence, the
group budget bounds one call's prompt.

**Why this is safe.** Findings are parsed per file from a single response and
already flow through `split_existing_duplicates` and the run-duplicate collapse,
so a larger group does not change downstream handling. The counter-argument is
attention dilution — a model reviewing 8 files in one call may cover each less
thoroughly than 8 calls would. That is exactly what `should_run_gap_audit` exists
to catch, and it is worth measuring: compare findings-per-file at 60 KB vs 250 KB
on the same PR before committing to a number.

---

### Solution 5 — Keep the cacheable prefix byte-identical

**Impact: converts part of the 11.4M uncached tokens into 10%-priced cache reads. Status: open.**

**Evidence.** `cacheRead` is already 29.0M against 11.4M uncached — the provider
caches aggressively — but `cacheWrite` is 0 and a third of input still bills at
full price.

**Root cause.** Prompt caching matches on a **prefix**. The scan templates put the
static instructions first and `REPO_CONTEXT` at
`prompts/reviewer_application.md:77` / `reviewer_tester.md:87`, which is good.
But the block that follows varies per call in ways that do not need to:
`ReviewContext::rendered` (`dashboard.rs:942`) is identical across a run, while
the coverage ledger is rendered *path-scoped* per call
(`coverage_ledger.render_for_paths`) and the relationship summary varies per
group. Any variation early in the prompt invalidates the cache for everything
after it.

**What to change.** Order every review prompt as: static template → invariant
run context (`ReviewContext::rendered`, byte-identical for all calls in the run)
→ per-call material (path-scoped ledger, relationship edges, file evidence,
existing comments). This is a reordering of the `substitute_review_prompt`
placeholders in the four templates, not a logic change.

**Why this is safe.** No information is added or removed; only its position
changes. Verify with telemetry: `uncachedInput` should fall and `cacheRead` rise
on the second and subsequent calls of a run.

---

### Solution 6 — Send comment keys, not comment bodies

**Impact: up to 12 KB per file-scan call on heavily reviewed PRs. Status: open.**

**Root cause.** `build_review_group_prompt_with_relationships` embeds
`file.existing_comments` up to `REVIEW_COMMENTS_MAX_BYTES` (12 KB,
`dashboard.rs:109`) in every scan prompt, so the model can avoid repeating
existing feedback. But the pipeline **already** enforces that deterministically:
`split_existing_duplicates` matches new findings against `file.existing_keys`
(line + normalised title) and drops the duplicates regardless of what the model
does — the "Already posted" rows on the Done screen.

**What to change.** Replace the full comment bodies in the prompt with the
`existing_keys` list (`line: title`, one per line). Keep the deterministic dedup
exactly as it is — it stays the actual guarantee.

**Why this is safe.** The prompt's instruction is "do not repeat these"; a title
and line number is sufficient for that instruction and is what the deterministic
filter compares anyway. The full body was never load-bearing.

---

### Solution 7 — Do not pay full price for a retry

**Impact: ~0.5–1M tokens on a run like the measured one. Status: partly addressed.**

**Evidence.** 18 of 610 calls (3%) had `retryRole: "full-rescan"` — a complete
re-send of a 100–365 KB prompt, at full uncached price, because the first
response did not parse.

**Already addressed (commit `0118163d`).**
`review_failure_repeats_on_rescan` (`src/tui/app.rs`) now suppresses the full
rescan for failures that would repeat deterministically, keeping it for transient
ones.

**What remains.** Confirm every scan path tries the cheap reformat first. The
reformat prompt (`prompts/reviewer_reformat.md`, 642 bytes + the previous output)
exists and is wired for File, Coverage and Merged profiles
(`build_review_reformat_prompt`, `dashboard.rs:8374`) — check that the gap-audit
and verification paths, which have no reformat profile, either gain one or are
explicitly exempt because their outputs are small enough to re-request cheaply.

---

### Solution 8 — Gap audit (already fixed; recorded for completeness)

**Status: already addressed.**

The measured run built a **2.87 MB** gap-audit prompt that the model rejected in
239 ms with zero tokens billed — the pass contributed nothing, and the run showed
a "Failed — primary findings kept" row. The cause was an uncapped cross-group
relationship summary, which grows with the square of the changed files.
`REVIEW_RELATIONSHIP_EDGES_MAX_BYTES` (16 KB, `dashboard.rs:138`) plus per-field
caps in `build_review_gap_audit_prompt` now bound the whole prompt to roughly
75 KB.

Worth adding: assert the assembled prompt length before dispatch and record a
warning row if a builder ever exceeds its budget again, so the next occurrence
surfaces as a wisetree-side error rather than a silent model rejection.

---

## 3. Suggested order

| # | Solution | Est. saving | Risk | Depends on |
|---|---|---|---|---|
| 1 | Coverage stops re-sending app bodies | 8–12M | low | — |
| 2 | Narrow the verification gate + routing | 3–5M | low | — |
| 5 | Cache-prefix ordering | part of 11.4M uncached | very low | — |
| 6 | Comment keys instead of bodies | ~12 KB/call | very low | — |
| 4 | Larger group budget | 1–1.5M | medium (dilution) | measure first |
| 3 | Density-based evidence | multiplies 1 and 4 | high (anchors) | own test pass |
| 7 | Retry audit | 0.5–1M | low | — |

Solutions 1, 2, 5 and 6 are independent and additive: together they should take a
run like the measured one from **40M input tokens to roughly 20M**, and its
verification stage from 6.2 hours to well under 2, without changing what the
reviewer looks at.

## 4. Why the PR command still beats the `/reviewer` skill

The skill reviews serially in one session: it pays for repo context once and
reuses the KV cache for the whole review, but it cannot parallelise and its
coverage of a large diff degrades as context fills. The PR command trades that
single-session cache for independent, parallel, per-file calls — which is why
**call count**, not prompt size, is the thing to optimise here. Every solution
above either removes a call or removes duplicated payload from the calls that
remain; none of them give up the parallelism that motivated the command.

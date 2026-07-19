## Task Description

Implement improvements #1 through #11 from `REVIEWER_IMPROVEMENTS.md`, then harden and validate Wisetree's Review Pull Request pipeline through approved Sections 12–19. The additional work closes deterministic recall gaps, guarantees complete coverage evidence, improves deduplication and test-context fidelity, batches large reviews efficiently, makes finding revisions adaptive, removes duplicated prompt evidence, and adds a repeatable benchmark against the canonical `reviewer` skill.

Sections 1–11 map one-to-one to the original numbered improvements and commits. Sections 12–19 map one-to-one to the approved follow-up implementation order. Before starting a section, re-read its acceptance criteria; after implementation, update this plan and run the section's focused tests before moving forward. Run the complete format/lint/test gate after the final section.

The following invariants apply to every section:

- Walkthrough, posting, summary, report, anchor validation, no-op stripping, and both dedup layers remain deterministic Rust.
- Exactly one scan per run may raise missing-test-coverage findings.
- Digests and inline content never remove the model's permission to read real repository files.
- All five categories, both per-file profiles, the merged profile, and the reference-tables tier remain available.
- The machine-parsed marker block, finding chunk delimiters, and header order remain byte-for-byte compatible.
- The existing untracked `prompt.md` is user-owned and must not be modified or committed.

**Complexity**: 20 points

The scope is epic-sized and is split into nineteen sequential, independently verifiable sections. Later sections may rely only on completed earlier sections.

---

## Implementation Sections

#### Section 1 — Cache-align prompt templates

**Goal**: Make the static prompt instructions a shared immutable prefix while preserving every instruction and the exact output contract.

**Files**: `prompts/reviewer_application.md`, `prompts/reviewer_tester.md`, `prompts/reviewer_coverage.md`, `src/services/dashboard.rs`, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [x] Every variable input block is located after all static role, category, quality, and output-contract text in each prompt.
- [x] Prompt-builder tests prove all placeholders are substituted and the static prefix precedes variable file/comment/revision content.
- [x] Parser tests continue to accept the exact marker/chunk/header contract.
- [x] `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all` pass.
- [x] The improvement is committed alone with the required #1 title and Status line.

**Edge cases**:

- [x] Empty existing comments, feedback, and previous finding still render explicit empty-state values.
- [x] User-controlled text containing placeholder-like words cannot corrupt later substitutions.

---

#### Section 2 — Per-scan token telemetry

**Goal**: Measure each scan call and persist bounded run history without letting telemetry failures affect review completion.

**Files**: `src/services/dashboard.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, `src/services/mod.rs`, `src/constants.rs`, a focused telemetry service file if needed, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [x] Each app, tester, and coverage call records scan identity, prompt bytes, input/output token usage, duration, and parsed finding count.
- [x] The Done report includes one concise aggregate usage row with call count and available token totals.
- [x] Only a bounded number of recent review runs is persisted under `~/.wisetree/`, using camelCase serialization and best-effort I/O.
- [x] Tests cover usage parsing, aggregation/formatting, retention bounds, and the Done-report row.
- [x] The local opencode usage source is exercised against fixtures matching the discovered session/step token shape.
- [x] The full format/lint/test gate passes and the improvement is committed alone with its #2 Status line.

**Edge cases**:

- [x] Missing, locked, changed-schema, or unreadable opencode state records `tokens: unavailable` while retaining prompt bytes, duration, and findings.
- [x] Failed and retried calls remain measurable as distinct paid calls without double-counting findings from discarded output.
- [x] Telemetry persistence failure never fails or delays the review workflow.

---

#### Section 3 — Cheap retry through reformatting

**Goal**: Repair malformed model output with a small contract-only call before paying for one full re-scan fallback.

**Files**: `prompts/reviewer_reformat.md`, `src/services/dashboard.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [x] A parse failure retains the raw model output and dispatches a reformat call containing only the contract and malformed output.
- [x] Valid reformatted output returns through the normal parser, validation, dedup, telemetry, and scan-pool paths.
- [x] A failed reformat triggers exactly one full re-scan; only failure of that fallback produces the final Failed row.
- [x] Tests cover file and coverage scans for reformat success, reformat failure followed by full-scan success, and terminal failure.
- [x] The full format/lint/test gate passes and the improvement is committed alone with its #3 Status line.

**Edge cases**:

- [x] A clean `NO-FINDINGS` block recovered by reformatting is treated as a successful empty scan.
- [x] Late retry events after cancellation or scan settlement remain ignored.
- [x] Reformatting cannot gain write permissions or require repository/diff context.

---

#### Section 4 — Adaptive merged scan

**Goal**: Route budget-sized PRs through one combined app-and-coverage call while preserving the existing split pipeline for larger diffs.

**Files**: `prompts/reviewer.md`, `src/services/dashboard.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [x] A named focus-budget constant inside the specified 25–30 KB range deterministically selects merged or split mode from total annotated-diff bytes.
- [x] Merged mode runs tester scans plus exactly one sentinel-style merged call that judges app categories and missing coverage across all changed files.
- [x] Split mode retains per-app/per-test file scans plus exactly one coverage pass.
- [x] The generalized multi-file parser accepts all five categories in merged mode while coverage parsing remains category-pinned in split mode.
- [x] Tests cover boundary routing, tests-only PRs, sentinel accounting/retry/failure, file mapping/anchor validation, prompt profile content, and single coverage ownership in both modes.
- [x] The full format/lint/test gate passes and the improvement is committed alone with its #4 Status line.

**Edge cases**:

- [x] A diff exactly equal to the focus budget uses merged mode; one byte over uses split mode.
- [x] Tests-only changes do not create a coverage owner beyond tester scans.
- [x] Empty or fully skipped diffs retain the existing no-changes behavior.

---

#### Section 5 — Pre-digest shared repository context

**Goal**: Read repository conventions and changed-directory inventories once during preparation and inject a bounded digest into every scan profile.

**Files**: `src/services/dashboard.rs`, all four active reviewer prompt files, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [x] Preparation builds a shared review context containing section-aware capped convention docs and bounded directory file inventories.
- [x] Merged, split-app, tester, and split-coverage prompt builders receive the same relevant context without violating cache-aligned layout.
- [x] Prompts narrow default exploratory reads but explicitly retain targeted full-file, sibling, test, and convention-file access when evidence is ambiguous.
- [x] Tests cover document priority/capping, line-safe truncation, directory inventory construction, missing files/directories, and prompt injection.
- [x] The full format/lint/test gate passes and the improvement is committed alone with its #5 Status line.

**Edge cases**:

- [x] Missing, unreadable, non-UTF-8, or oversized convention files degrade to the available digest rather than failing preparation.
- [x] Inventories remain bounded for very large directories and do not traverse outside the worktree.

---

#### Section 6 — Inline capped full-file content

**Goal**: Supply full changed-file bodies by default when bounded, and require a real-file read before structural findings on larger files.

**Files**: `src/services/dashboard.rs`, all four active reviewer prompt files, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [x] A named per-file cap inside the specified range controls whether full content is attached to `ReviewFile`.
- [x] Merged, split-app, and tester prompts include capped full content alongside authoritative numbered hunks.
- [x] Files over the cap include no body and every applicable prompt carries the mandatory structural-read trigger while retaining normal read access.
- [x] Merged/split routing continues to count annotated hunks only, not the inlined body.
- [x] Tests cover below-cap, exact-cap, over-cap, unreadable/binary content, and prompt behavior in each profile.
- [x] The full format/lint/test gate passes and the improvement is committed alone with its #6 Status line.

**Edge cases**:

- [x] Deleted or unavailable new-side files do not fail preparation.
- [x] Non-UTF-8 content is not inlined and follows the conservative read-on-demand path.

---

#### Section 7 — Feed test-quality findings to coverage

**Goal**: Gate the sole coverage-owning scan until tester scans settle, then provide their conclusions as compact evidence.

**Files**: `src/services/dashboard.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, `prompts/reviewer.md`, `prompts/reviewer_coverage.md`, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [x] Tester scans are dispatched before or alongside split app scans, while the merged/coverage sentinel cannot dispatch until every tester scan reaches a terminal state.
- [x] Tester findings are formatted into bounded one-line evidence entries and passed only to the sole coverage owner.
- [x] Merged and coverage prompts instruct the model to verify flagged weak tests before counting them as coverage.
- [x] Tester and split-app prompts remain forbidden from creating missing-coverage findings.
- [x] Tests cover ordering/gating, out-of-order completions, tester failures, empty feeds, feed formatting, and exactly-one coverage ownership.
- [x] The full format/lint/test gate passes and the improvement is committed alone with its #7 Status line.

**Edge cases**:

- [x] A failed tester scan still settles the gate and remaining valid tester findings are forwarded.
- [x] PRs with no changed test files dispatch the coverage owner without an unnecessary wait.
- [x] Duplicate tester findings do not create duplicate coverage owners or direct coverage comments.

---

#### Section 8 — Slim test diffs to scenario skeletons

**Goal**: Give the coverage owner only scenario declarations and assertions from changed tests while leaving full tester scans unchanged.

**Files**: `src/services/dashboard.rs`, `prompts/reviewer.md`, `prompts/reviewer_coverage.md`, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [x] A conservative multi-language extractor retains recognized scenario/test declarations and assertion lines from annotated test diffs.
- [x] Merged and split-coverage prompt builders use test skeletons plus nearby test-path inventory; tester prompts still receive full annotated test diffs.
- [x] Coverage prompts explicitly retain permission to read the full test file when the skeleton is ambiguous.
- [x] Tests cover representative Rust, Ruby, Python, and JavaScript/TypeScript test shapes, line numbers, unknown syntax fallback, and profile separation.
- [x] The full format/lint/test gate passes and the improvement is committed alone with its #8 Status line.

**Edge cases**:

- [x] Unrecognized test frameworks degrade conservatively without silently removing all coverage evidence.
- [x] Multiline assertions and nested scenario names retain enough context or trigger the full-file read allowance.

---

#### Section 9 — Compact existing-comment keys

**Goal**: Replace full existing review bodies in model prompts with concise line/title keys while retaining Rust's authoritative dedup data.

**Files**: `src/services/dashboard.rs`, active reviewer prompt builders/templates as needed, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [x] Existing Wisetree comments render as one compact line containing anchor and normalized title; human comments contribute only a bounded first line.
- [x] Deterministic `ExistingFindingKey` extraction and `split_duplicate_findings` behavior remain unchanged.
- [x] Per-file and multi-file prompts consume the compact representation and no longer include full comment bodies by default.
- [x] Tests cover Wisetree comments, human comments, missing anchors, multiline/long bodies, and unchanged deterministic dedup.
- [x] The full format/lint/test gate passes and the improvement is committed alone with its #9 Status line.

**Edge cases**:

- [x] Blank or heading-only human comments do not create misleading keys.
- [x] Multiple comments with the same first line remain harmless because model context is advisory and Rust dedup is authoritative.

---

#### Section 10 — Expand deterministic skip rules

**Goal**: Exclude only provably judgment-free changes and report every exclusion reason on the Done screen.

**Files**: `src/services/dashboard.rs`, `src/tui/screens/review_pr.rs` if scan metadata/reporting requires it, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [x] Pure 100%-similarity renames/moves are represented as visible skipped entries rather than scans or silent omissions.
- [x] Comment/blank-only diffs are skipped only when every changed line is conservatively recognized for that language.
- [x] `.svg`, `.pdf`, and oversized fixture changes receive explicit deterministic skip reasons.
- [x] Skipped bytes are excluded before merged/split mode routing.
- [x] Tests cover every new skip class plus near-miss cases that must still be reviewed.
- [x] The full format/lint/test gate passes and the improvement is committed alone with its #10 Status line.

**Edge cases**:

- [x] Renames with edits, low similarity, or ambiguous diff metadata are scanned.
- [x] Mixed comment/code changes, unknown languages, doc comments with executable examples, and ambiguous fixtures are scanned.
- [x] Binary/deleted files retain existing safe handling without duplicate report rows.

---

#### Section 11 — Focus the “Other” revision call

**Goal**: Revise one finding with a dedicated prompt containing the previous finding, feedback, and only local anchor context.

**Files**: `prompts/reviewer_revise.md`, `src/services/dashboard.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [x] The revision path no longer renders either per-file scan profile or a whole diff.
- [x] The dedicated prompt receives the exact output contract, previous finding, user feedback, target path, and the anchored hunk with at most ±20 surrounding lines.
- [x] When #6 supplied capped full content it remains available to the revision; repository read access is explicitly retained.
- [x] Revision parsing, anchor validation, no-op stripping, walkthrough replacement, and failure fallback remain deterministic and unchanged.
- [x] Tests cover single-line/range/file-level anchors, hunk boundaries, full-content presence/absence, placeholder substitution, and revision failure returning to the existing finding.
- [x] The full format/lint/test gate passes and the improvement is committed alone with its #11 Status line.

**Edge cases**:

- [x] Missing or invalid anchors use a conservative local context without including unrelated hunks.
- [x] Feedback containing prompt marker text cannot corrupt the static output contract or substitution order.
- [x] A revision never introduces extra findings or changes the coverage-owner count.

---

#### Section 12 — Review convention- and security-relevant skipped changes

**Goal**: Ensure renames, text SVGs, and comment-only changes remain reviewable while preserving deterministic skips for genuinely content-free changes.

**Files**: `src/services/dashboard.rs`, `prompts/reviewer.md`, `prompts/reviewer_application.md`, tests in `src/services/dashboard.rs`, `PLAN.md`

**Acceptance criteria**:

- [x] Pure renames/moves remain in the review input with their source and destination metadata so Convention findings can be raised.
- [x] UTF-8 SVG changes remain reviewable for Security and Convention findings; opaque PDFs remain visibly skipped.
- [x] Blank-only diffs remain skipped, while comment-only diffs remain reviewable.
- [x] Oversized fixture handling remains conservative and visible in the Done report.
- [x] Focused skip/parse/prompt tests pass.

**Edge cases**:

- [x] Renames with edits remain normal reviewable diffs and are not represented twice.
- [x] Binary SVGs or unreadable new-side content degrade to diff-only review without preparation failure.
- [x] Deletion-only comments remain reviewable when their removal could matter.

---

#### Section 13 — Guarantee complete coverage review input

**Goal**: Ensure every changed application file reaches exactly one coverage judgment without being silently removed by a global prompt cap.

**Files**: `src/services/dashboard.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, `prompts/reviewer_coverage.md`, tests in the same Rust modules, `PLAN.md`

**Acceptance criteria**:

- [x] Coverage work is partitioned into deterministic budget-bounded groups whose application-file sets are disjoint and complete.
- [x] Every group carries an explicit complete changed-file manifest and the relevant test evidence available for that group.
- [x] No global truncation can remove a changed application file without creating another coverage group for it.
- [x] Coverage findings remain anchored and deduplicated through the existing deterministic pipeline.
- [x] Tests cover boundary sizes, oversized single files, tail files, retries, failures, and scan settlement.

**Edge cases**:

- [x] Tests-only PRs continue to avoid unnecessary coverage groups.
- [x] One application file larger than the group budget is reviewed alone with an explicit truncation/read instruction.
- [x] A failed group does not block unrelated groups from completing and is reported by name.

---

#### Section 14 — Preserve distinct findings that share an anchor

**Goal**: Deduplicate only semantically equivalent findings instead of discarding independent concerns merely because they use the same line.

**Files**: `src/services/dashboard.rs`, `src/tui/screens/review_pr.rs`, tests in those modules, `PLAN.md`

**Acceptance criteria**:

- [x] Findings on the same file and line remain distinct when their category, normalized concern, and proposed fix differ.
- [x] Findings with equivalent normalized concerns or identical concrete fixes still collapse deterministically.
- [x] Existing-comment dedup remains backward compatible with posted Wisetree comments.
- [x] Focused dedup and walkthrough-order tests pass.

**Edge cases**:

- [x] File-level findings without anchors deduplicate only when their concerns match.
- [x] Same title with different capitalization or whitespace remains one concern.
- [x] Same fix proposed from different anchors in the same file remains one actionable finding.

---

#### Section 15 — Preserve complete test evidence and prioritize tester findings

**Goal**: Retain meaningful multiline scenario/assertion evidence and make the bounded tester-to-coverage feed deterministic and importance-ordered.

**Files**: `src/services/dashboard.rs`, `src/tui/screens/review_pr.rs`, `prompts/reviewer.md`, `prompts/reviewer_coverage.md`, tests in the Rust modules, `PLAN.md`

**Acceptance criteria**:

- [x] Test evidence retains bounded surrounding lines for recognized scenarios and complete multiline assertions when deterministically identifiable.
- [x] Partial or ambiguous extraction falls back to the full annotated test diff rather than presenting misleading fragments.
- [x] Tester findings are sorted by severity and stable diff order before the evidence cap is applied.
- [x] Truncated tester evidence reports how many findings were omitted.
- [x] Tests cover Rust, Ruby, Python, JavaScript/TypeScript, partial syntax, ordering, and truncation.

**Edge cases**:

- [x] Removed scenarios and assertions remain visible.
- [x] Unknown frameworks retain their full diff.
- [x] Asynchronous scan completion order does not change the rendered evidence.

---

#### Section 16 — Batch application and tester scans by focus budget

**Goal**: Reduce per-call overhead and restore cross-file reasoning in split mode by scanning related files in deterministic budget-bounded groups.

**Files**: `src/services/dashboard.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, reviewer prompt templates, tests in the Rust modules, `PLAN.md`

**Acceptance criteria**:

- [x] Split-mode application files are grouped under the focus budget, preferring same-directory files and keeping oversized files alone.
- [x] Test files are grouped independently under the focus budget in both merged and split modes.
- [x] Group prompts preserve each profile's responsibilities and emit exact file paths for every finding.
- [x] Scan concurrency, retry, telemetry, dedup, ordering, failure reporting, and completion accounting work per group.
- [x] Tests cover grouping determinism, budget boundaries, mixed app/test PRs, oversized files, retries, and findings from multiple files.

**Edge cases**:

- [x] A one-file group behaves identically to the current per-file path.
- [x] Tests-only and application-only PRs settle without sentinel deadlocks.
- [x] No file is omitted or scanned by two groups of the same profile.

---

#### Section 17 — Escalate revisions adaptively and support deletion suggestions

**Goal**: Keep ordinary revisions cheap while expanding context when user feedback requires another hunk/file and making direct deletions one-click applicable.

**Files**: `prompts/reviewer_revise.md`, `src/services/dashboard.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, tests in the Rust modules, `PLAN.md`

**Acceptance criteria**:

- [x] Focused revision remains the default for local wording or fix changes.
- [x] Feedback that explicitly references broader context receives all target-file hunks and repository read guidance without rescanning unrelated PR files.
- [x] A failed focused revision can retry once with expanded target-file context.
- [x] Empty suggestion blocks round-trip as intentional line/range deletions and render as GitHub deletion suggestions.
- [x] Tests cover local, expanded, failed-then-expanded, invalid-anchor, and deletion revision flows.

**Edge cases**:

- [x] Expansion never migrates a finding to an unrelated file silently.
- [x] File-level findings cannot emit invalid inline deletion suggestions.
- [x] Empty accidental model sections remain distinguishable from intentional deletions.

---

#### Section 18 — Eliminate duplicated full-file and diff evidence

**Goal**: Provide bounded files as one authoritative numbered full-content representation instead of separately sending duplicate full content and diff hunks.

**Files**: `src/services/dashboard.rs`, all active reviewer prompt templates, tests in `src/services/dashboard.rs`, `PLAN.md`

**Acceptance criteria**:

- [x] Inlined files render one numbered full-current-file view with changed lines visibly marked and authoritative new-side anchors preserved.
- [x] Removed lines remain available as a compact separate block when needed to understand behavior deletion.
- [x] Large/unavailable/non-UTF-8 files retain annotated hunks and targeted read instructions.
- [x] Merged, grouped application, tester, coverage, and revision profiles use the unified representation consistently.
- [x] Prompt-size tests demonstrate that changed current lines are not duplicated for bounded files.

**Edge cases**:

- [x] New, empty, deletion-only, and no-newline files render valid evidence.
- [x] Multiple hunks and adjacent additions preserve correct line markers.
- [x] Unified evidence remains within existing prompt and routing budgets.

---

#### Section 19 — Benchmark token efficiency and review accuracy

**Goal**: Add a repeatable, non-posting benchmark that compares the Pull Request review pipeline with a skill-like baseline on labeled defects and token dimensions.

**Files**: benchmark fixtures and scripts under a focused repository-owned location, reviewer service/test helpers as needed, documentation, `PLAN.md`

**Acceptance criteria**:

- [x] The benchmark corpus covers every finding category plus deletion-only changes, renames, SVG security, cross-file issues, multiline assertions, false-positive traps, and suggestion quality.
- [x] A deterministic evaluator reports precision, recall, F1, anchor validity, and suggestion applicability from captured outputs.
- [x] Token reporting separates uncached input, cache reads/writes, output, reasoning, logical total, and available cost data.
- [x] The benchmark has a fixture-only CI mode that requires no model/network and an explicitly invoked live comparison mode using the same model/thinking level.
- [x] Documentation states the exact command, limitations, and evidence threshold required before claiming superiority over the skill.

**Edge cases**:

- [x] Missing model credentials or telemetry produces an explicit unavailable result, never fabricated zeroes.
- [x] Stochastic live runs support repetition and aggregate statistics.
- [x] Benchmark execution never posts comments, submits reviews, or modifies fixture repositories.

---

## Progress Tracker

| Section | Name | Status |
|---------|------|--------|
| 1 | Cache-align prompt templates | ✅ Done |
| 2 | Per-scan token telemetry | ✅ Done |
| 3 | Cheap retry through reformatting | ✅ Done |
| 4 | Adaptive merged scan | ✅ Done |
| 5 | Pre-digest shared repository context | ✅ Done |
| 6 | Inline capped full-file content | ✅ Done |
| 7 | Feed test-quality findings to coverage | ✅ Done |
| 8 | Slim test diffs to scenario skeletons | ✅ Done |
| 9 | Compact existing-comment keys | ✅ Done |
| 10 | Expand deterministic skip rules | ✅ Done |
| 11 | Focus the “Other” revision call | ✅ Done |
| 12 | Review convention- and security-relevant skipped changes | ✅ Done |
| 13 | Guarantee complete coverage review input | ✅ Done |
| 14 | Preserve distinct findings that share an anchor | ✅ Done |
| 15 | Preserve complete test evidence and prioritize tester findings | ✅ Done |
| 16 | Batch application and tester scans by focus budget | ✅ Done |
| 17 | Escalate revisions adaptively and support deletion suggestions | ✅ Done |
| 18 | Eliminate duplicated full-file and diff evidence | ✅ Done |
| 19 | Benchmark token efficiency and review accuracy | ✅ Done |

Sections 12–19 are pre-approved by the developer and may execute sequentially without additional approval gates, while retaining a focused test gate and progress update after each section.

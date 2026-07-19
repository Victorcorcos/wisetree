## Task Description

Implement improvements #1 through #11 from `REVIEWER_IMPROVEMENTS.md`, harden the Review Pull Request pipeline through completed Sections 12–19, then execute the planned accuracy-and-efficiency program in Sections 20–29. The new program must make Review discover real code flaws more accurately than the canonical `reviewer` skill with statistically defensible evidence while preserving Review's deterministic workflow and making it a clear end-to-end logical-token winner.

Sections 1–11 map one-to-one to the original numbered improvements and commits. Sections 12–19 map one-to-one to the completed follow-up implementation order. Sections 20–29 are new, sequential sessions: first eliminate known recall and token-multiplication gaps, then improve evidence selection and cross-file reasoning, then add selective verification and a global omission audit, and finally prove superiority with leakage-free live evaluation. Before starting a section, re-read its acceptance criteria; after implementation, update this plan and run the section's focused tests before moving forward. Run the complete format/lint/test gate after the final section.

The following invariants apply to every section:

- Walkthrough, posting, summary, report, anchor validation, no-op stripping, and both dedup layers remain deterministic Rust.
- Exactly one review role may raise missing-test-coverage findings; budget partitioning may create several disjoint calls owned by that role, but no changed behavior may be judged twice.
- Digests and inline content never remove the model's permission to read real repository files.
- All five categories, both per-file profiles, the merged profile, and the reference-tables tier remain available.
- The machine-parsed marker block, finding chunk delimiters, and header order remain byte-for-byte compatible.
- The existing untracked `prompt.md` is user-owned and must not be modified or committed.

Additional invariants for Sections 20–29:

- Every changed application behavior, including whole-file deletion, is assigned to exactly one primary discovery group; no deterministic skip may make a potentially actionable security, behavior, convention, or test regression invisible.
- Related evidence is supplied once to its owning group wherever possible. Repeated full test suites, full files, or repository instructions across coverage groups require a measured justification.
- Relationship discovery, evidence extraction, routing, token accounting, validation, and posting remain deterministic Rust; models judge code, not pipeline bookkeeping.
- Accuracy improvements must not move ordinary walkthrough, posting, summary, or reporting work back into model turns.
- Provider prompt caching is reported as a cost dimension, not counted as a logical-token reduction.
- Benchmark adapters never receive labels, expected findings, adjudication notes, or hidden holdout metadata.
- Review and the skill run with the same model, thinking level, repository state, tool permissions, timeout policy, and repetition set.
- No document or user-facing message may claim superiority until Section 29's preregistered accuracy and token thresholds pass on the held-out live corpus.

**Complexity**: 34 points

The scope is epic-sized and is split into twenty-nine sequential, independently verifiable sections. Later sections may rely only on completed earlier sections.

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

#### Section 20 — Eliminate deterministic review blind spots

**Goal**: Ensure potentially actionable changes reach discovery instead of being discarded before any model can judge them.

**Files**: `src/services/dashboard.rs`, reviewer prompt templates as needed, focused parser/routing tests, benchmark cases, `PLAN.md`

**Acceptance criteria**:

- [x] Whole-file text deletions remain represented with the old path, complete removed-line evidence within a bound, and file-level-comment support when no right-side anchor exists.
- [x] Deleted application, policy/configuration, and test files participate in application and coverage judgments instead of becoming `NoChanges` or disappearing from the final report.
- [x] Lockfiles, generated code, vendored code, snapshots, minified assets, and oversized fixtures are no longer unconditionally treated as judgment-free; each class is either conservatively reviewed, routed to a cheap specialized check, or skipped only after a deterministic proof that the change cannot carry an actionable concern.
- [x] Test classification covers common unit, integration, end-to-end, behavior-specification, and framework patterns including `e2e/`, `integration/`, `features/`, `*.cy.*`, and `.feature`, with project-configured patterns taking precedence.
- [x] Ambiguous file classifications degrade to review rather than skip, while every retained skip remains visible with its reason.
- [x] Tests cover whole-file deletion of authorization code, whole-test deletion, dependency/lockfile changes, generated/security-sensitive near misses, nonstandard test layouts, and truly judgment-free binary changes.

**Edge cases**:

- [x] Deleted binary files remain visible but do not fabricate text evidence.
- [x] A pure rename, a rename plus deletion, and delete-and-recreate at the same path remain distinguishable.
- [x] File-level deletion findings never attempt an invalid GitHub suggestion block.

---

#### Section 21 — Establish an end-to-end logical-token budget

**Goal**: Make token efficiency a measured product constraint before adding accuracy calls, and eliminate known evidence multiplication in split mode.

**Files**: `src/services/dashboard.rs`, `src/services/review_telemetry.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, reviewer prompts, benchmark capture schema/evaluator, tests, `PLAN.md`

**Acceptance criteria**:

- [x] Telemetry records uncached input, cache read, cache write, output, reasoning, logical total, cost, scan role, retry role, and end-to-end review totals without collapsing dimensions.
- [x] Cached tokens remain part of logical-token totals; cache savings are reported separately as cost/latency information.
- [x] Split coverage groups receive only tests related to their application behaviors plus a compact global test manifest; the entire changed test set is not repeated in every group.
- [x] Bounded full-file evidence, changed-test evidence, convention context, and tester findings each have a deterministic ownership/dedup key so prompt construction can prove when the same bytes are resent.
- [x] Prompt-building tests expose per-role and per-evidence-kind byte totals and fail when an unapproved repeated-evidence budget is exceeded.
- [x] The ordinary Post/Edit/Skip walkthrough, posting, summary, and final report remain zero-model-turn operations.
- [ ] Initial live measurements establish token baselines by PR shape: small merged, medium merged, large split, test-heavy, finding-heavy, clean, and revision-heavy.

**Edge cases**:

- [x] Missing or partial provider telemetry makes the comparison unavailable rather than treating dimensions as zero.
- [x] Retries and expanded revisions are counted as paid calls even when their results are discarded.
- [x] One test legitimately related to several application groups uses a compact shared assertion digest or targeted read instead of repeated full-file payloads.

---

#### Section 22 — Extract behavior- and symbol-level evidence

**Goal**: Give each reviewer the complete relevant implementation context for changed behavior without paying for unrelated portions of large files.

**Files**: a focused reviewer-evidence module under `src/services/`, `src/services/dashboard.rs`, reviewer prompts, language fixtures/tests, `PLAN.md`

**Acceptance criteria**:

- [x] Each changed hunk maps, when deterministically possible, to its complete enclosing function/method, class/type/module boundary, and changed symbol identity.
- [x] Evidence includes directly referenced local types, constants, error variants, and bounded caller/callee context when those relationships are required to judge the change.
- [x] Removed behavior is preserved alongside the current symbol so regression and deleted-guard analysis remains possible.
- [x] Supported languages use existing lightweight parsers or minimal proven extraction; no broad parsing framework is introduced without measured necessity.
- [x] When extraction is unavailable or ambiguous, the scan must read the real full changed file before completing that file's discovery judgment, not only after it has already noticed a structural finding.
- [x] Small files continue using one authoritative numbered full-file representation; changed current lines are never duplicated in separate hunks.
- [x] Tests cover nested functions/classes, multiple hunks in one symbol, macros/decorators/attributes, overloads, partial syntax, non-UTF-8 files, and extraction fallback.

**Edge cases**:

- [x] A change between symbols receives bounded module context without being arbitrarily assigned to the wrong function.
- [x] Generated or templated languages fall back safely without silently clipping relevant code.
- [x] Symbol evidence preserves authoritative new-side anchors and old-side deletion context.

---

#### Section 23 — Build relationship-aware review groups

**Goal**: Replace directory-only batching with deterministic groups that expose the cross-file relationships where real defects occur.

**Files**: reviewer-evidence/routing modules, `src/services/dashboard.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, reviewer prompts, tests, `PLAN.md`

**Acceptance criteria**:

- [x] The review graph connects changed files and symbols through imports, module references, direct calls, shared types/constants, configuration consumers, schema/model consumers, rename history, and implementation-to-test relationships.
- [x] Primary application groups prefer strong dependency relationships over directory proximity while remaining within the measured focus budget.
- [x] Cross-layer changes such as schema → model → service → controller and configuration → worker/consumer are visible together when their relationship is relevant.
- [x] Every changed application behavior belongs to exactly one primary discovery group; overlapping context is bounded evidence, not duplicate ownership.
- [x] Oversized connected components split deterministically with explicit cross-group edge summaries so no relationship disappears at the boundary.
- [x] Tester groups and coverage ownership remain separate and cannot create duplicate missing-coverage findings.
- [x] Tests cover same-directory unrelated files, cross-directory related files, cyclic dependencies, fan-in/fan-out, partial migrations, renames, and deterministic ordering.

**Edge cases**:

- [x] Dynamic or unresolved imports remain visible as uncertain edges rather than false certainty.
- [x] A disconnected one-file PR behaves like the existing focused path.
- [x] Very high-degree shared utility files do not pull the entire PR into one unbounded group.

---

#### Section 24 — Make relevant-test inspection mandatory and complete

**Goal**: Ensure coverage decisions use the actual assertions protecting each changed behavior while avoiding repeated test payloads.

**Files**: reviewer-evidence/routing modules, `src/services/dashboard.rs`, merged/coverage/tester prompts, tests, `PLAN.md`

**Acceptance criteria**:

- [x] Every changed application behavior has an explicit coverage-ledger entry naming its related changed and unchanged tests, or recording that no relevant test was found.
- [x] Test discovery uses naming, imports/references, framework configuration, repository conventions, and the relationship graph rather than only the changed file's directory.
- [x] The coverage owner receives concrete scenario names and complete meaningful assertions that would fail when the changed behavior regresses.
- [x] When deterministic evidence cannot establish whether an existing test protects the behavior, the coverage owner must perform a targeted real-file read before emitting or suppressing the finding.
- [x] Tester findings about weak assertions, excessive internal mocking, flakiness, and implementation-detail tests are connected to the exact behavior they fail to protect.
- [x] Each related test body is supplied to one owning specialist; other consumers receive bounded assertion digests or targeted-read references.
- [x] Tests cover separate test roots, parameterized tests, generated cases, shared examples, integration/e2e suites, weak assertions, unchanged regression tests, and no-test repositories.

**Edge cases**:

- [x] One test protecting several behaviors records each relationship without duplicating its full body.
- [x] A test named like the implementation but asserting unrelated behavior does not count as coverage.
- [x] Deleted tests update the ledger as lost protection rather than ordinary absent evidence.

---

#### Section 25 — Selectively verify discovered findings and fixes

**Goal**: Increase precision and proposed-fix correctness with a bounded adversarial verification call only where its value justifies its tokens.

**Files**: a verifier prompt, `src/services/dashboard.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, telemetry, parser/state-machine tests, `PLAN.md`

**Acceptance criteria**:

- [x] Verification is mandatory for Critical/High, Security, file-level, invalid-anchor, cross-group, and suggestion-bearing findings; straightforward lower-severity prose findings may bypass it under an explicit deterministic policy.
- [x] The verifier receives exactly one candidate concern, its local behavior/symbol evidence, applicable relationship/test/convention evidence, and proposed replacement—never the whole PR by default.
- [x] The verifier emits exactly `CONFIRMED`, `REJECTED_FALSE_POSITIVE`, or `REVISE` with structured reasoning and, for `REVISE`, one corrected finding.
- [x] Rejected candidates never reach the walkthrough; revised candidates pass normal anchor, dedup, no-op, and suggestion validation.
- [x] Direct suggestions are checked against the targeted replacement range and, when safely available, parsed/formatted or applied in an isolated temporary patch followed by the smallest relevant compile/test check.
- [x] Verification tokens are recorded separately and remain within the end-to-end budget established in Section 21.
- [x] Tests cover correct findings, plausible false positives, stale assumptions, invalid fixes, deletion suggestions, broad refactors, verifier parse failures, and retry limits.

**Edge cases**:

- [x] Verification failure keeps the candidate explicitly unverified rather than silently confirming it; the user sees the status or the finding is conservatively withheld according to policy.
- [x] Isolated suggestion validation never mutates the user's worktree or posts to GitHub.
- [x] Two findings sharing an anchor remain independently verified when their concerns differ.

---

#### Section 26 — Add a compact global omission audit

**Goal**: Recover cross-group recall after focused discovery by auditing the complete behavior/relationship ledger without resending full diffs.

**Files**: a global-gap-audit prompt, reviewer-evidence data structures, `src/services/dashboard.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, telemetry/tests, `PLAN.md`

**Acceptance criteria**:

- [x] The audit receives the complete changed-file/symbol/behavior manifest, important cross-group edges, coverage-ledger status, deterministic skip decisions, and compact summaries of already discovered findings.
- [x] Its only discovery responsibility is omissions caused by decomposition: partial migrations, missing consumers, cross-directory inconsistencies, duplicated behavior, shotgun surgery, configuration/schema mismatches, and behaviors absent from the finding/coverage ledger.
- [x] The audit first identifies a concrete missing relationship and requests only the targeted evidence needed to confirm it; it does not receive every full file or test by default.
- [x] New audit findings pass the same verifier and deterministic validation pipeline as primary findings.
- [x] Existing findings are never rephrased into duplicates, and the audit cannot become a second general missing-coverage owner.
- [x] Clean and single-group PRs use a smaller manifest-only audit or skip it under a measured deterministic policy.
- [x] Tests cover omissions spanning two and several groups, clean PRs, duplicate suppression, targeted expansion, failed audit calls, and token-budget enforcement.

**Edge cases**:

- [x] A relationship edge summarized under the context cap remains addressable by stable identifier.
- [x] Audit failure never discards findings already discovered and verified.
- [x] Tests-only PRs do not create an unnecessary application gap audit.

---

#### Section 27 — Implement real leakage-free benchmark adapters

**Goal**: Execute Review and the canonical `reviewer` skill end-to-end under identical read-only conditions instead of comparing hand-authored captures.

**Files**: benchmark adapters/scripts under `benchmarks/reviewer/`, evaluator/capture schema, read-only harness fixtures, documentation, tests, `PLAN.md`

**Acceptance criteria**:

- [x] Repository-owned executable adapters exist for both Review and `/Users/victorcorcos/Desktop/repositories/skills/skills/reviewer/SKILL.md` and implement the same capture protocol.
- [x] The runner produces a redacted review-input corpus containing no expected findings, labels, adjudication notes, severity hints, or hidden metadata before invoking either adapter.
- [x] Both adapters use the same model, thinking level, repository snapshot, tool permissions, context files, timeouts, and repetition identifiers.
- [x] Adapters exercise discovery, finding revision/suggestion production when applicable, and deterministic end-to-end workflow tokens without posting comments, submitting reviews, or modifying source fixtures.
- [x] Every model and tool turn contributes to logical-token totals; missing dimensions remain unavailable.
- [x] Capture provenance includes workflow commit, skill hash, model/provider identifier, thinking level, corpus hash, environment version, and timestamps.
- [x] Tests prove label redaction, read-only enforcement, adapter parity, side-effect rejection, missing-credential behavior, and complete repetition coverage.

**Edge cases**:

- [x] The installed and repository skill copies must hash-match or the benchmark stops with an explicit mismatch.
- [x] Adapter crashes preserve prior repetitions but make the comparison incomplete rather than selectively dropping bad runs.
- [x] Tool reads and context expansion are included in usage instead of only counting the initial prompt.

---

#### Section 28 — Build a representative blinded accuracy corpus

**Goal**: Measure real defect discovery and fix quality across production-like PRs without overfitting to a tiny synthetic checklist.

**Files**: versioned public corpus metadata, private/held-out corpus integration, adjudication tooling/docs, evaluator, benchmark tests, `PLAN.md`

**Acceptance criteria**:

- [x] The evaluation contains at least 100 cases, targeting 200, spanning small/medium/large, clean/defective, test-heavy, finding-heavy, split-mode, and revision-heavy PR shapes.
- [x] Cases cover all five categories plus whole-file deletions, cross-directory/cross-layer defects, authorization/security regressions, partial migrations, large-file structural flaws, unconventional test layouts, weak/missing tests, dependency changes, and false-positive traps.
- [x] Ground truth combines real historical reviewed defects with controlled seeded mutations whose intended behavior and minimal correct fix are independently documented.
- [x] A private held-out subset is inaccessible to implementation prompts and routine development; public fixtures test evaluator mechanics only.
- [ ] At least two blind adjudicators classify semantic correctness, severity, duplicate equivalence, and proposed-fix quality, with disagreements resolved and agreement reported.
- [x] Suggestion quality is measured by isolated application plus formatter/parser/build/test outcomes where applicable, not exact-string equality alone.
- [x] Metrics include precision, recall, F1, severity-weighted recall, Critical/High recall, false positives per PR, cross-file recall, test-gap recall, anchor validity, suggestion correctness/applicability, and end-to-end logical tokens by PR shape.

**Edge cases**:

- [x] Convention cases include the repository evidence needed to make the expected convention objectively inferable.
- [x] Multiple valid fixes are accepted through semantic/application adjudication rather than one golden string.
- [x] Clean cases are large enough and varied enough to expose systematic over-reporting.

---

#### Section 29 — Prove and continuously guard joint superiority

**Goal**: Permit the claim that Review is more accurate and more token-efficient only after preregistered, statistically defensible live results pass together.

**Files**: benchmark evaluator/reporting, CI/nightly workflows as appropriate, benchmark documentation, user-facing Review documentation, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [x] Before running the held-out comparison, preregister thresholds and analysis: Review F1 at least `+0.05`, recall at least `+0.08`, precision no worse than `-0.01`, Critical/High recall non-inferior and preferably at least `95%`, suggestion success at least `+0.05`, and median end-to-end logical tokens at least `25%` lower than the skill.
- [x] Both workflows run at least ten paired repetitions per case on the same model/thinking/environment, preserving every stochastic result.
- [x] Paired confidence intervals or an appropriate paired bootstrap demonstrate the accuracy and token improvements at 95% confidence instead of relying on point estimates.
- [x] Token superiority is reported by PR-shape bucket as well as overall; no major bucket may show Review materially more expensive without an explicit failed gate.
- [x] Accuracy reports separate discovery from delivery mechanics so anchor/dedup success cannot masquerade as flaw-detection success.
- [ ] A human-reviewed evidence packet confirms that matched findings identify the intended defects and proposed fixes genuinely solve them.
- [x] Only after every accuracy and token gate passes are superiority statements added to documentation or UI; otherwise reports name the failed dimensions and the claim remains disabled.
- [x] Deterministic evaluator/fixture checks run in normal CI, while credentialed stochastic comparisons run on an explicitly controlled schedule and alert on material regression.

**Edge cases**:

- [x] Provider/model upgrades create a new baseline rather than being compared across incompatible environments.
- [x] Missing telemetry, incomplete repetitions, adjudicator disagreement above the allowed threshold, or holdout leakage automatically invalidates the claim.
- [x] A later regression below any preregistered threshold removes or marks the superiority claim stale until revalidated.

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
| 20 | Eliminate deterministic review blind spots | ✅ Done |
| 21 | Establish an end-to-end logical-token budget | 🟡 Live baseline pending |
| 22 | Extract behavior- and symbol-level evidence | ✅ Done |
| 23 | Build relationship-aware review groups | ✅ Done |
| 24 | Make relevant-test inspection mandatory and complete | ✅ Done |
| 25 | Selectively verify discovered findings and fixes | ✅ Done |
| 26 | Add a compact global omission audit | ✅ Done |
| 27 | Implement real leakage-free benchmark adapters | ✅ Done |
| 28 | Build a representative blinded accuracy corpus | 🟡 Blind adjudication pending |
| 29 | Prove and continuously guard joint superiority | 🟡 Private live proof pending |

Sections 1–20 and 22–27 are implemented. Section 21's code awaits live token baselines; Section 28 awaits two-person blind adjudication; and Section 29 awaits the preregistered private live comparison plus its resolved human evidence packet. Section 29 remains the sole authority for declaring that Review is jointly more accurate and more token-efficient than the canonical skill; the claim is currently disabled.

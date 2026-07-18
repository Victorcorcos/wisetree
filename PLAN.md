## Task Description

Implement improvements #1 through #11 from `REVIEWER_IMPROVEMENTS.md` in strict order for Wisetree's Review Pull Request pipeline. The work reduces repeated prompt and diff cost, adds measurable per-call telemetry, bounds retry cost, introduces adaptive merged-versus-split scanning, restores full-file and cross-call review context, trims deterministic inputs, expands safe skip rules, and makes finding revisions focused.

Each section below maps one-to-one to one numbered improvement and one commit. Before starting a section, re-read its authoritative spec item. After implementation, add that item's required `Status` line to `REVIEWER_IMPROVEMENTS.md`, update this plan, run the complete format/lint/test gate, and commit only that section. Improvement #12 remains intentionally unimplemented.

The following invariants apply to every section:

- Walkthrough, posting, summary, report, anchor validation, no-op stripping, and both dedup layers remain deterministic Rust.
- Exactly one scan per run may raise missing-test-coverage findings.
- Digests and inline content never remove the model's permission to read real repository files.
- All five categories, both per-file profiles, the merged profile, and the reference-tables tier remain available.
- The machine-parsed marker block, finding chunk delimiters, and header order remain byte-for-byte compatible.
- The existing untracked `prompt.md` is user-owned and must not be modified or committed.

**Complexity**: 20 points

The scope is epic-sized and should be split. The specification's eleven sequential improvements are the split: each is a separately reviewable, fully gated commit, and later sections may rely only on completed earlier sections.

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

- [ ] A named focus-budget constant inside the specified 25–30 KB range deterministically selects merged or split mode from total annotated-diff bytes.
- [ ] Merged mode runs tester scans plus exactly one sentinel-style merged call that judges app categories and missing coverage across all changed files.
- [ ] Split mode retains per-app/per-test file scans plus exactly one coverage pass.
- [ ] The generalized multi-file parser accepts all five categories in merged mode while coverage parsing remains category-pinned in split mode.
- [ ] Tests cover boundary routing, tests-only PRs, sentinel accounting/retry/failure, file mapping/anchor validation, prompt profile content, and single coverage ownership in both modes.
- [ ] The full format/lint/test gate passes and the improvement is committed alone with its #4 Status line.

**Edge cases**:

- [ ] A diff exactly equal to the focus budget uses merged mode; one byte over uses split mode.
- [ ] Tests-only changes do not create a coverage owner beyond tester scans.
- [ ] Empty or fully skipped diffs retain the existing no-changes behavior.

---

#### Section 5 — Pre-digest shared repository context

**Goal**: Read repository conventions and changed-directory inventories once during preparation and inject a bounded digest into every scan profile.

**Files**: `src/services/dashboard.rs`, all four active reviewer prompt files, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [ ] Preparation builds a shared review context containing section-aware capped convention docs and bounded directory file inventories.
- [ ] Merged, split-app, tester, and split-coverage prompt builders receive the same relevant context without violating cache-aligned layout.
- [ ] Prompts narrow default exploratory reads but explicitly retain targeted full-file, sibling, test, and convention-file access when evidence is ambiguous.
- [ ] Tests cover document priority/capping, line-safe truncation, directory inventory construction, missing files/directories, and prompt injection.
- [ ] The full format/lint/test gate passes and the improvement is committed alone with its #5 Status line.

**Edge cases**:

- [ ] Missing, unreadable, non-UTF-8, or oversized convention files degrade to the available digest rather than failing preparation.
- [ ] Inventories remain bounded for very large directories and do not traverse outside the worktree.

---

#### Section 6 — Inline capped full-file content

**Goal**: Supply full changed-file bodies by default when bounded, and require a real-file read before structural findings on larger files.

**Files**: `src/services/dashboard.rs`, all four active reviewer prompt files, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [ ] A named per-file cap inside the specified range controls whether full content is attached to `ReviewFile`.
- [ ] Merged, split-app, and tester prompts include capped full content alongside authoritative numbered hunks.
- [ ] Files over the cap include no body and every applicable prompt carries the mandatory structural-read trigger while retaining normal read access.
- [ ] Merged/split routing continues to count annotated hunks only, not the inlined body.
- [ ] Tests cover below-cap, exact-cap, over-cap, unreadable/binary content, and prompt behavior in each profile.
- [ ] The full format/lint/test gate passes and the improvement is committed alone with its #6 Status line.

**Edge cases**:

- [ ] Deleted or unavailable new-side files do not fail preparation.
- [ ] Non-UTF-8 content is not inlined and follows the conservative read-on-demand path.

---

#### Section 7 — Feed test-quality findings to coverage

**Goal**: Gate the sole coverage-owning scan until tester scans settle, then provide their conclusions as compact evidence.

**Files**: `src/services/dashboard.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, `prompts/reviewer.md`, `prompts/reviewer_coverage.md`, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [ ] Tester scans are dispatched before or alongside split app scans, while the merged/coverage sentinel cannot dispatch until every tester scan reaches a terminal state.
- [ ] Tester findings are formatted into bounded one-line evidence entries and passed only to the sole coverage owner.
- [ ] Merged and coverage prompts instruct the model to verify flagged weak tests before counting them as coverage.
- [ ] Tester and split-app prompts remain forbidden from creating missing-coverage findings.
- [ ] Tests cover ordering/gating, out-of-order completions, tester failures, empty feeds, feed formatting, and exactly-one coverage ownership.
- [ ] The full format/lint/test gate passes and the improvement is committed alone with its #7 Status line.

**Edge cases**:

- [ ] A failed tester scan still settles the gate and remaining valid tester findings are forwarded.
- [ ] PRs with no changed test files dispatch the coverage owner without an unnecessary wait.
- [ ] Duplicate tester findings do not create duplicate coverage owners or direct coverage comments.

---

#### Section 8 — Slim test diffs to scenario skeletons

**Goal**: Give the coverage owner only scenario declarations and assertions from changed tests while leaving full tester scans unchanged.

**Files**: `src/services/dashboard.rs`, `prompts/reviewer.md`, `prompts/reviewer_coverage.md`, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [ ] A conservative multi-language extractor retains recognized scenario/test declarations and assertion lines from annotated test diffs.
- [ ] Merged and split-coverage prompt builders use test skeletons plus nearby test-path inventory; tester prompts still receive full annotated test diffs.
- [ ] Coverage prompts explicitly retain permission to read the full test file when the skeleton is ambiguous.
- [ ] Tests cover representative Rust, Ruby, Python, and JavaScript/TypeScript test shapes, line numbers, unknown syntax fallback, and profile separation.
- [ ] The full format/lint/test gate passes and the improvement is committed alone with its #8 Status line.

**Edge cases**:

- [ ] Unrecognized test frameworks degrade conservatively without silently removing all coverage evidence.
- [ ] Multiline assertions and nested scenario names retain enough context or trigger the full-file read allowance.

---

#### Section 9 — Compact existing-comment keys

**Goal**: Replace full existing review bodies in model prompts with concise line/title keys while retaining Rust's authoritative dedup data.

**Files**: `src/services/dashboard.rs`, active reviewer prompt builders/templates as needed, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [ ] Existing Wisetree comments render as one compact line containing anchor and normalized title; human comments contribute only a bounded first line.
- [ ] Deterministic `ExistingFindingKey` extraction and `split_duplicate_findings` behavior remain unchanged.
- [ ] Per-file and multi-file prompts consume the compact representation and no longer include full comment bodies by default.
- [ ] Tests cover Wisetree comments, human comments, missing anchors, multiline/long bodies, and unchanged deterministic dedup.
- [ ] The full format/lint/test gate passes and the improvement is committed alone with its #9 Status line.

**Edge cases**:

- [ ] Blank or heading-only human comments do not create misleading keys.
- [ ] Multiple comments with the same first line remain harmless because model context is advisory and Rust dedup is authoritative.

---

#### Section 10 — Expand deterministic skip rules

**Goal**: Exclude only provably judgment-free changes and report every exclusion reason on the Done screen.

**Files**: `src/services/dashboard.rs`, `src/tui/screens/review_pr.rs` if scan metadata/reporting requires it, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [ ] Pure 100%-similarity renames/moves are represented as visible skipped entries rather than scans or silent omissions.
- [ ] Comment/blank-only diffs are skipped only when every changed line is conservatively recognized for that language.
- [ ] `.svg`, `.pdf`, and oversized fixture changes receive explicit deterministic skip reasons.
- [ ] Skipped bytes are excluded before merged/split mode routing.
- [ ] Tests cover every new skip class plus near-miss cases that must still be reviewed.
- [ ] The full format/lint/test gate passes and the improvement is committed alone with its #10 Status line.

**Edge cases**:

- [ ] Renames with edits, low similarity, or ambiguous diff metadata are scanned.
- [ ] Mixed comment/code changes, unknown languages, doc comments with executable examples, and ambiguous fixtures are scanned.
- [ ] Binary/deleted files retain existing safe handling without duplicate report rows.

---

#### Section 11 — Focus the “Other” revision call

**Goal**: Revise one finding with a dedicated prompt containing the previous finding, feedback, and only local anchor context.

**Files**: `prompts/reviewer_revise.md`, `src/services/dashboard.rs`, `src/tui/app.rs`, `src/tui/screens/review_pr.rs`, `REVIEWER_IMPROVEMENTS.md`, `PLAN.md`

**Acceptance criteria**:

- [ ] The revision path no longer renders either per-file scan profile or a whole diff.
- [ ] The dedicated prompt receives the exact output contract, previous finding, user feedback, target path, and the anchored hunk with at most ±20 surrounding lines.
- [ ] When #6 supplied capped full content it remains available to the revision; repository read access is explicitly retained.
- [ ] Revision parsing, anchor validation, no-op stripping, walkthrough replacement, and failure fallback remain deterministic and unchanged.
- [ ] Tests cover single-line/range/file-level anchors, hunk boundaries, full-content presence/absence, placeholder substitution, and revision failure returning to the existing finding.
- [ ] The full format/lint/test gate passes and the improvement is committed alone with its #11 Status line.

**Edge cases**:

- [ ] Missing or invalid anchors use a conservative local context without including unrelated hunks.
- [ ] Feedback containing prompt marker text cannot corrupt the static output contract or substitution order.
- [ ] A revision never introduces extra findings or changes the coverage-owner count.

---

## Progress Tracker

| Section | Name | Status |
|---------|------|--------|
| 1 | Cache-align prompt templates | ✅ Done |
| 2 | Per-scan token telemetry | ✅ Done |
| 3 | Cheap retry through reformatting | ✅ Done |
| 4 | Adaptive merged scan | ⬚ Pending |
| 5 | Pre-digest shared repository context | ⬚ Pending |
| 6 | Inline capped full-file content | ⬚ Pending |
| 7 | Feed test-quality findings to coverage | ⬚ Pending |
| 8 | Slim test diffs to scenario skeletons | ⬚ Pending |
| 9 | Compact existing-comment keys | ⬚ Pending |
| 10 | Expand deterministic skip rules | ⬚ Pending |
| 11 | Focus the “Other” revision call | ⬚ Pending |

Improvement #12 is excluded from this plan. Its go/no-go decision requires production telemetry that cannot be established in this implementation session.

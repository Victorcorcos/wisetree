# Reviewer benchmark

This benchmark compares Review Pull Request command captures with reviewer-skill captures. It is read-only: the corpus contains no PR number or remote target, adapters receive `--read-only` plus `WISETREE_BENCHMARK_READ_ONLY=1`, and captures declaring side effects are rejected.

## Fixture-only CI mode

This command requires no model, credentials, network, or repository mutation:

```bash
cargo run --quiet --bin reviewer_benchmark -- \
  benchmarks/reviewer/corpus.json \
  benchmarks/reviewer/captured/pipeline.fixture.json \
  benchmarks/reviewer/captured/skill.fixture.json
```

The checked-in captures and eight-case `corpus.json` are evaluator fixtures, not evidence that either workflow is superior. The generated `corpus.public.json` contains 118 public development cases: 108 controlled mutation/clean cases across six PR shapes and ten regressions made by reversing real reviewed fixes. Ground truth is independently documented in `CORPUS_SPECS.md` or the referenced reviewed fix commit. Regenerate and validate it deterministically with:

```bash
cargo run --quiet --bin reviewer_corpus -- generate benchmarks/reviewer/corpus.public.json
cargo run --quiet --bin reviewer_corpus -- check benchmarks/reviewer/corpus.public.json
```

The evaluator rejects missing cases, duplicate case/repetition pairs, mismatched repetition sets, and incomplete coverage. It reports precision, recall, F1, severity-weighted and Critical/High recall, false positives per PR, cross-file and test-gap recall, anchor validity, semantic suggestion correctness, logical-token dimensions, cost, and per-shape accuracy/token buckets. Public accepted-fix patterns avoid requiring one exact golden string. Blind adjudication supplies semantic/application/formatter/parser/build/test outcomes for live suggestions.

## Live comparison mode

The repository owns executable adapters for the Pull Request command and the canonical `/Users/victorcorcos/Desktop/repositories/skills/skills/reviewer/SKILL.md` workflow. The runner passes the exact same model, thinking level, timeout, read-only fixture snapshot, tool permissions, and repetition IDs to each:

```bash
bash benchmarks/reviewer/live_compare.sh \
  openai/gpt-5.6-terra high 5 240
```

Before either adapter runs, it creates a label-free `*.review-input.json` containing only case IDs and review diffs. Tags, expected findings, valid anchors, notes, severities, and any other evaluator metadata are removed by a whitelist serializer. The adapters stop if the installed and repository reviewer-skill hashes differ. They use opencode's read-only `plan` agent in an immutable temporary fixture, never call `gh` or posting APIs, and hash the fixture and source corpus before and after each run.

The Review adapter enters the production Review service: relationship-aware tester/application grouping, merged or split coverage ownership, malformed-output retry, compact omission audit, selective verification/revision, and deterministic dedup all run exactly as they do before the UI walkthrough. For controlled parity, the adapter pins Review's `strong`, `balanced`, and `utility` profiles to the same model and thinking value supplied to the skill adapter; production runs retain their configured profile routing, which is recorded per call in Review telemetry. The skill adapter executes the hash-verified canonical skill under the same fixture, model, thinking, permissions, and per-turn timeout. Posting and review submission are excluded from both because they are deterministic delivery side effects, not discovery work.

Each capture records the workflow commit plus source-tree hash, canonical skill hash, harness executable, model/provider, thinking level, source and redacted corpus hashes, tool permissions, timeout, environment versions, start/completion timestamps, every parsed finding, and complete uncached-input/cache-read/cache-write/output/reasoning/cost telemetry for every production model/tool turn. Missing credentials, a model crash, missing telemetry, a parse failure, or a mutation preserves prior repetitions in an explicitly incomplete capture and exits nonzero. The deterministic evaluator refuses incomplete, side-effecting, or provenance-mismatched live captures. Successful captures are preserved under `captured/live-<UTC timestamp>/`.

Repeated runs are aggregated across repetitions. Every stochastic repetition remains a separate `runs` entry—no result selection is permitted.

## Private holdout and blind adjudication

The private holdout is deliberately absent from this repository and must contain at least 100 cases, giving at least 218 public-plus-private cases. Its access-controlled path and preregistered BLAKE3 hash are supplied only to the controlled run:

```bash
WISETREE_REVIEWER_HOLDOUT_CORPUS=/controlled/holdout.json \
WISETREE_REVIEWER_HOLDOUT_HASH=<preregistered-blake3> \
bash benchmarks/reviewer/live_compare.sh openai/gpt-5.6-terra high 10 240
```

The validator rejects a holdout inside the repository, a hash mismatch, or fewer than 100 private cases. Adapters still receive only the whitelist-redacted case ID, diff, and objective context files.

Create a workflow-blinded adjudication packet after both captures, keeping the generated `.map.json` away from adjudicators:

```bash
cargo run --quiet --bin reviewer_adjudication -- packet \
  /controlled/holdout.json pipeline.json skill.json blind-packet.json
cargo run --quiet --bin reviewer_adjudication -- resolve \
  blind-packet.json adjudicator-a.json adjudicator-b.json resolution.json report.json
```

Two distinct adjudicators must attest `blind: true` and decide every candidate's semantic correctness, severity, duplicate equivalence, and proposed-fix quality. Fix checks separately record isolated application, formatter, parser, build, and tests as `pass`, `fail`, `not-applicable`, or `unavailable`. All disagreements require a resolution; the report publishes raw agreement and Cohen's kappa.

## Claim threshold and limitations

Do not claim that the Pull Request command is “better” from public fixtures, point estimates, or a single live run. `preregistration.json` freezes the analysis before private-holdout execution. A claim requires at least ten paired repetitions per case and all of these gates at once:

- F1 at least `+0.05`, recall at least `+0.08`, and precision no worse than `-0.01`;
- Review Critical/High recall at least `95%` and non-inferior to the skill;
- human-adjudicated suggestion success at least `+0.05`;
- median end-to-end logical tokens at least `25%` lower, with no PR-shape bucket more than `5%` worse;
- paired 95% bootstrap lower bounds above zero for F1, recall, and token reduction;
- complete, identical provenance/telemetry and a resolved blind evidence packet with agreement at or above the preregistered floors.

Run the final fail-closed gate only after adjudication:

```bash
cargo run --quiet --bin reviewer_superiority -- gate \
  benchmarks/reviewer/preregistration.json /controlled/holdout.json \
  pipeline.json skill.json adjudication-report.json blind-packet.map.json \
  superiority-status.json
```

The output separates discovery accuracy from delivery mechanics such as anchors and duplicates. A provider/model, thinking, environment, corpus, skill, or workflow change produces a new baseline key; it is never compared as though it were the old baseline. Missing telemetry, incomplete pairing, holdout leakage, low agreement, or any failed gate writes a disabled claim state and exits nonzero.

The eight-case fixture corpus is intentionally tiny. The 118-case public corpus is broad enough to test mechanics and expose common over-reporting, but it is visible to developers and therefore cannot prove superiority. Only the access-controlled held-out corpus and completed blind evidence packet may feed Section 29's claim gate.

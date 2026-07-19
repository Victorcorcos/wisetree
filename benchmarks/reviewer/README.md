# Reviewer benchmark

This benchmark compares Review Pull Request command captures with reviewer-skill captures. It is read-only: the corpus contains no PR number or remote target, adapters receive `--read-only` plus `WISETREE_BENCHMARK_READ_ONLY=1`, captures declaring side effects are rejected, and temporary live outputs are deleted on exit.

## Fixture-only CI mode

This command requires no model, credentials, network, or repository mutation:

```bash
cargo run --quiet --bin reviewer_benchmark -- \
  benchmarks/reviewer/corpus.json \
  benchmarks/reviewer/captured/pipeline.fixture.json \
  benchmarks/reviewer/captured/skill.fixture.json
```

The checked-in captures are evaluator fixtures, not evidence that either workflow is superior. Each corpus case contains the synthetic unified diff that both workflows must review; tags and expected findings are evaluator labels, not review input. The evaluator rejects missing cases, duplicate case/repetition pairs, mismatched repetition sets, and captures that do not cover every case in every repetition. It reports precision, recall, F1, anchor validity, suggestion applicability, and separate uncached-input, cache-read, cache-write, output, reasoning, logical-total, median logical tokens per repetition, and cost dimensions. Missing telemetry is printed as `unavailable`; it is never converted to zero.

## Live comparison mode

Use trusted executable adapters for the Pull Request command and `/Users/victorcorcos/Desktop/repositories/skills/reviewer` workflow. Both adapters must implement the protocol below. The runner passes the exact same model and thinking level to each:

```bash
bash benchmarks/reviewer/live_compare.sh \
  /absolute/path/to/pipeline-benchmark-adapter \
  /absolute/path/to/skill-benchmark-adapter \
  openai/gpt-5.2-codex high 5
```

Each adapter receives `--corpus`, `--model`, `--thinking`, `--repetitions`, `--output`, and `--read-only`. It must review only the synthetic corpus, never call `git`, `gh`, posting APIs, or modify fixtures, and write the same capture shape as `captured/*.fixture.json`. Every run needs a case ID, repetition number, parsed findings, and nullable token/cost dimensions. It must set `sideEffects` to `false`; the evaluator refuses any other value. A credentials, model, or telemetry failure must exit nonzero or record null dimensions so the runner reports the result as unavailable.

Repeated runs are aggregated across repetitions. Preserve each stochastic repetition as a separate `runs` entry—do not select the best run.

## Claim threshold and limitations

Do not claim that the Pull Request command is “better” from fixture captures or a single live run. A superiority claim requires all of the following on the same corpus, model, thinking level, and environment:

- at least five repetitions per case for both workflows;
- lower median logical token total for the Pull Request command;
- F1 at least 0.05 higher, with precision, recall, anchor validity, and suggestion applicability none lower;
- complete token dimensions for both workflows, or an explicit claim limited to the dimensions actually available;
- manual confirmation that matched findings identify the intended defect and that exact suggestions apply cleanly.

The corpus is intentionally small and synthetic. It exercises all five categories, deletion-only changes, renames, SVG security, cross-file reasoning, multiline assertions, false-positive traps, and direct suggestion quality, but it does not represent every language, framework, or production PR distribution.

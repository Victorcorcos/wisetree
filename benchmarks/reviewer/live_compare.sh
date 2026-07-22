#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo_root"

if [[ $# -lt 3 || $# -gt 4 ]]; then
  echo "usage: bash benchmarks/reviewer/live_compare.sh <model> <thinking> <repetitions> [timeout-seconds]" >&2
  exit 2
fi

model=$1
thinking=$2
repetitions=$3
timeout_seconds=${4:-240}
pipeline_adapter=benchmarks/reviewer/review_adapter.sh
skill_adapter=benchmarks/reviewer/skill_adapter.sh
corpus=${WISETREE_REVIEWER_HOLDOUT_CORPUS:-benchmarks/reviewer/corpus.public.json}

if [[ -n "${WISETREE_REVIEWER_HOLDOUT_CORPUS:-}" ]]; then
  if [[ -z "${WISETREE_REVIEWER_HOLDOUT_HASH:-}" ]]; then
    echo "live benchmark unavailable: WISETREE_REVIEWER_HOLDOUT_HASH is required for a private holdout" >&2
    exit 3
  fi
  cargo run --quiet --bin reviewer_corpus -- \
    validate-private "$corpus" "$WISETREE_REVIEWER_HOLDOUT_HASH"
fi

if [[ ! -x "$pipeline_adapter" || ! -x "$skill_adapter" ]]; then
  echo "live benchmark unavailable: both adapter paths must be executable" >&2
  exit 3
fi
if [[ ! "$repetitions" =~ ^[1-9][0-9]*$ || ! "$timeout_seconds" =~ ^[1-9][0-9]*$ ]]; then
  echo "live benchmark unavailable: repetitions and timeout must be positive integers" >&2
  exit 3
fi

run_id=$(date -u +%Y%m%dT%H%M%SZ)
capture_dir=${WISETREE_REVIEWER_CAPTURE_DIR:-"benchmarks/reviewer/captured/live-$run_id"}
mkdir -p "$capture_dir"

run_adapter() {
  local adapter=$1
  local output=$2
  if ! WISETREE_BENCHMARK_READ_ONLY=1 "$adapter" \
    --corpus "$corpus" \
    --model "$model" \
    --thinking "$thinking" \
    --repetitions "$repetitions" \
    --timeout-seconds "$timeout_seconds" \
    --output "$output" \
    --read-only; then
    echo "live benchmark unavailable: adapter failed (check model credentials and telemetry)" >&2
    exit 4
  fi
}

run_adapter "$pipeline_adapter" "$capture_dir/pipeline.json"
run_adapter "$skill_adapter" "$capture_dir/skill.json"

cargo run --quiet --bin reviewer_benchmark -- \
  "$corpus" \
  "$capture_dir/pipeline.json" \
  "$capture_dir/skill.json"

echo "live captures: $capture_dir"

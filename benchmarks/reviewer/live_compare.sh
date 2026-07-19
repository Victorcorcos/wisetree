#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 5 ]]; then
  echo "usage: bash benchmarks/reviewer/live_compare.sh <pipeline-adapter> <skill-adapter> <model> <thinking> <repetitions>" >&2
  exit 2
fi

pipeline_adapter=$1
skill_adapter=$2
model=$3
thinking=$4
repetitions=$5
corpus=benchmarks/reviewer/corpus.json

if [[ ! -x "$pipeline_adapter" || ! -x "$skill_adapter" ]]; then
  echo "live benchmark unavailable: both adapter paths must be executable" >&2
  exit 3
fi
if [[ ! "$repetitions" =~ ^[1-9][0-9]*$ ]]; then
  echo "live benchmark unavailable: repetitions must be a positive integer" >&2
  exit 3
fi

capture_dir=$(mktemp -d)
trap 'rm -rf "$capture_dir"' EXIT

run_adapter() {
  local adapter=$1
  local output=$2
  if ! WISETREE_BENCHMARK_READ_ONLY=1 "$adapter" \
    --corpus "$corpus" \
    --model "$model" \
    --thinking "$thinking" \
    --repetitions "$repetitions" \
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

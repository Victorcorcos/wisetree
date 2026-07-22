#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
exec cargo run --quiet --manifest-path "$repo_root/Cargo.toml" --bin reviewer_benchmark_adapter -- \
  --adapter review "$@"

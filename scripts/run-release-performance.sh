#!/bin/sh
set -eu

project=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
artifact_dir=${1:-"$project/performance/artifacts/latest"}
mkdir -p "$artifact_dir"

if [ "${LOCALREVIEW_PERF_SKIP_BUILD:-0}" != "1" ]; then
  cargo build --manifest-path "$project/Cargo.toml" --profile release \
    -p localreview-desktop --features perf-harness --bin localreview-perf
fi

iterations=${LOCALREVIEW_PERF_ITERATIONS:-24}
LOCALREVIEW_PERF_ITERATIONS=$iterations \
  "$project/target/release/localreview-perf" "$artifact_dir/controller.json"

# A longer second run provides enough active work for statistical sampling.
profile_iterations=${LOCALREVIEW_PERF_PROFILE_ITERATIONS:-160}
LOCALREVIEW_PERF_ITERATIONS=$profile_iterations \
  "$project/target/release/localreview-perf" "$artifact_dir/profile-controller.json" &
profile_pid=$!
sleep 0.2
sample "$profile_pid" 12 5 -file "$artifact_dir/stacks.out" >/dev/null 2>&1 || true
wait "$profile_pid"
python3 "$project/scripts/sample-to-flamegraph.py" \
  "$artifact_dir/stacks.out" "$artifact_dir/flamegraph.svg"

{
  echo "commit=$(git -C "$project" rev-parse HEAD)"
  echo "dirty_files=$(git -C "$project" status --porcelain | wc -l | tr -d ' ')"
  echo "arch=$(uname -m)"
  echo "os=$(sw_vers -productVersion)"
  echo "iterations=$iterations"
  echo "profile_iterations=$profile_iterations"
  echo "viewport_rows=220"
} > "$artifact_dir/environment.txt"

echo "Performance artifacts: $artifact_dir"

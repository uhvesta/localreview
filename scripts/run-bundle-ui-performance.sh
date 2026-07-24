#!/bin/sh
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
  echo "The packaged UI performance harness requires macOS." >&2
  exit 2
fi

project=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
artifact_dir=${1:-"$project/performance/artifacts/bundle-latest"}
mkdir -p "$artifact_dir"
fixture=$(mktemp -d "${TMPDIR:-/tmp}/localreview-ui-perf.XXXXXX")
data_dir="$fixture/data"
workspace="$fixture/workspace"
mkdir -p "$data_dir" "$workspace"

cleanup() {
  if [ -n "${frontend_pid:-}" ]; then kill "$frontend_pid" >/dev/null 2>&1 || true; fi
  if [ -n "${app_pid:-}" ]; then kill "$app_pid" >/dev/null 2>&1 || true; fi
  rm -rf "$fixture"
}
trap cleanup EXIT INT TERM

python3 - "$workspace/bench.rs" <<'PY'
from pathlib import Path
import sys
path = Path(sys.argv[1])
lines = ["pub fn target_symbol(value: usize) -> usize { value + 1 }\n"]
for index in range(1, 12000):
    lines.append(f"pub fn generated_{index}(value: usize) -> usize {{ target_symbol(value) + {index} }}\n")
path.write_text("".join(lines))
PY
git -C "$workspace" init -b main >/dev/null
git -C "$workspace" config user.email performance@example.invalid
git -C "$workspace" config user.name "LocalReview Performance"
git -C "$workspace" add bench.rs
git -C "$workspace" commit -m "performance base" >/dev/null
git -C "$workspace" switch -c performance-review >/dev/null
python3 - "$workspace/bench.rs" <<'PY'
from pathlib import Path
import sys
path = Path(sys.argv[1])
lines = path.read_text().splitlines(keepends=True)
current = []
for index, line in enumerate(lines):
    if index and index % 200 == 0:
        continue
    current.append(line)
    if index and index % 200 == 50:
        current.append(f"pub fn added_{index}(value: usize) -> usize {{ target_symbol(value) }}\n")
path.write_text("".join(current))
PY

if [ "${LOCALREVIEW_PERF_SKIP_BUILD:-0}" != "1" ]; then
  cargo build --manifest-path "$project/Cargo.toml" --profile release -p localreview-cli
  npx --yes @tauri-apps/cli@2 build \
    --config "$project/src-tauri/tauri.conf.json" \
    --config '{"productName":"LocalReviewPerf","identifier":"com.localreview.desktop.perf"}' \
    --bundles app --ci
fi

app="$project/target/release/bundle/macos/LocalReviewPerf.app/Contents/MacOS/localreview-desktop"
if [ ! -x "$app" ]; then
  echo "Release app bundle is missing: $app" >&2
  exit 1
fi

LOCALREVIEW_DATA_DIR="$data_dir" LOCALREVIEW_PERF_TRACE=1 \
  LOCALREVIEW_ALLOW_MULTIPLE_INSTANCES=1 \
  "$app" >"$artifact_dir/app-stdout.log" 2>"$artifact_dir/native-ipc.jsonl" &
app_pid=$!

ready=0
for _ in $(jot 120); do
  if ! kill -0 "$app_pid" >/dev/null 2>&1; then
    echo "Packaged LocalReview exited before its authenticated desktop endpoint became ready." >&2
    echo "This commonly means another instance rejected the harness; inspect $artifact_dir/native-ipc.jsonl." >&2
    exit 1
  fi
  doctor=$(
    LOCALREVIEW_DATA_DIR="$data_dir" "$project/target/release/localreview" --json doctor 2>/dev/null \
      || true
  )
  if echo "$doctor" | grep -q '"desktopReachable":true'; then
    ready=1
    break
  fi
  sleep 0.1
done
if [ "$ready" != "1" ]; then
  {
    echo "Packaged LocalReview did not become ready."
    echo "pid=$app_pid"
    ps -p "$app_pid" -o pid=,stat=,etime=,command= || true
    echo "final_doctor=$doctor"
    echo "data_directory_entries:"
    find "$data_dir" -maxdepth 3 -print 2>/dev/null || true
    echo "native_stderr:"
    tail -n 50 "$artifact_dir/native-ipc.jsonl" 2>/dev/null || true
    echo "app_stdout:"
    tail -n 50 "$artifact_dir/app-stdout.log" 2>/dev/null || true
  } | tee "$artifact_dir/readiness-failure.txt" >&2
  exit 1
fi

LOCALREVIEW_DATA_DIR="$data_dir" "$project/target/release/localreview" \
  open "$workspace" --base main >/dev/null

sample "$app_pid" 12 5 -file "$artifact_dir/stacks.out" >/dev/null 2>&1 &
sample_pid=$!
osascript -l JavaScript "$project/scripts/macos-ui-performance.jxa" "$app_pid" \
  > "$artifact_dir/frontend-wall.jsonl" 2>"$artifact_dir/frontend-stderr.log" &
frontend_pid=$!
frontend_status=124
for _ in $(jot 450); do
  if ! kill -0 "$frontend_pid" >/dev/null 2>&1; then
    if wait "$frontend_pid"; then frontend_status=0; else frontend_status=$?; fi
    frontend_pid=
    break
  fi
  sleep 0.1
done
if [ -n "$frontend_pid" ]; then
  kill "$frontend_pid" >/dev/null 2>&1 || true
  wait "$frontend_pid" >/dev/null 2>&1 || true
  frontend_pid=
fi
wait "$sample_pid" || true
python3 "$project/scripts/sample-to-flamegraph.py" \
  "$artifact_dir/stacks.out" "$artifact_dir/flamegraph.svg"

if [ "$frontend_status" != "0" ]; then
  {
    echo "Packaged app startup and CLI-to-desktop IPC succeeded."
    echo "Accessibility UI timing was unavailable (osascript status $frontend_status)."
    echo "macOS may be waiting for Accessibility permission, or AXEntireContents may be blocked."
    echo "Grant Accessibility access to the invoking terminal/Codex process, then rerun this script."
    echo "The watchdog stopped the UI driver after 45 seconds; native stacks remain usable."
    if [ -s "$artifact_dir/frontend-stderr.log" ]; then
      echo "Accessibility driver stderr:"
      cat "$artifact_dir/frontend-stderr.log"
    fi
  } > "$artifact_dir/frontend-unavailable.txt"
  cat "$artifact_dir/frontend-unavailable.txt" >&2
  exit 3
fi

echo "Packaged UI performance artifacts: $artifact_dir"

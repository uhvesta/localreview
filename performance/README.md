# LocalReview performance reproduction

Performance work is measured against the same native controller methods and
IPC response shapes used by the Tauri commands. The fixture is deterministic:
one 25,000-line Rust file with sparse additions, removals, modifications, and
repository-wide symbol references.

Run the release-profile native/serialization harness:

```sh
./scripts/run-release-performance.sh performance/artifacts/latest
./scripts/check-performance.py performance/artifacts/latest/controller.json
```

`controller.json` separates controller time, JSON serialization time, response
bytes, returned rows, syntax tokens, and omitted-block metadata. Presentation
operations request 220 rows, matching the desktop viewport request; the harness
does not use an artificial `u32::MAX` full-file response.

The command also profiles a longer active run with macOS `sample` and writes:

- `stacks.out` — raw sampled stacks.
- `flamegraph.svg` — self-contained visualization derived from those stacks.
- `profile-controller.json` — timings from the sampled run.
- `environment.txt` — commit, dirty state, OS, architecture, and iteration
  counts.

On macOS, measure the installed-style release bundle and optimistic Svelte
feedback separately:

```sh
./scripts/run-bundle-ui-performance.sh performance/artifacts/bundle-latest
```

This builds and launches the exact `.app` executable with an isolated durable
data directory, forwards a fixture through the release CLI, and drives the UI
through macOS Accessibility. Outputs include:

- `frontend-wall.jsonl` — click-to-optimistic-visual-state durations for one
  disclosure and expand/collapse-all.
- `native-ipc.jsonl` — opt-in native command durations emitted by the actual
  bundle (`LOCALREVIEW_PERF_TRACE=1`).
- `stacks.out` and `flamegraph.svg` — samples of the app process while the
  interactions run.

The Accessibility run requires the invoking terminal/automation host to have
macOS Accessibility permission. It never reads source through accessibility
and deletes its temporary repository/data directory at exit.

Guardrails are deliberately broad enough for shared CI hardware and narrow
enough to catch response-amplification regressions. They enforce bounded
220-row responses, payload caps for disclosure/highlighting, and release
latency ceilings for refresh and symbol search. They are not assertions about
one machine's minimum microsecond result.

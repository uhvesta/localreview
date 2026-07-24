#!/usr/bin/env python3
"""Stable release-fixture performance guardrails, not microsecond snapshots."""

from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: check-performance.py CONTROLLER.JSON", file=sys.stderr)
        return 2
    report = json.loads(Path(sys.argv[1]).read_text())
    operations = {item["name"]: item for item in report["operations"]}
    failures: list[str] = []

    def maximum(name: str, field: str, statistic: str, limit: int) -> None:
        actual = int(operations[name][field][statistic])
        if actual > limit:
            failures.append(
                f"{name}.{field}.{statistic} = {actual}, expected <= {limit}"
            )

    for name in (
        "individual_disclosure",
        "expand_collapse_all",
        "highlight_cold",
        "highlight_cached_viewport",
    ):
        if operations[name]["viewportRowsRequested"] != 220:
            failures.append(f"{name} did not exercise the real 220-row viewport")
        maximum(name, "responseRows", "max", 220)

    maximum("individual_disclosure", "nativeMicros", "median", 100_000)
    maximum("individual_disclosure", "responseBytes", "p95", 1_500_000)
    maximum("expand_collapse_all", "nativeMicros", "median", 100_000)
    maximum("expand_collapse_all", "responseBytes", "p95", 1_500_000)
    maximum("highlight_cached_viewport", "nativeMicros", "median", 25_000)
    maximum("highlight_cached_viewport", "responseBytes", "p95", 1_500_000)
    maximum("refresh", "nativeMicros", "p95", 750_000)
    maximum("symbol_navigation", "nativeMicros", "p95", 350_000)

    if failures:
        print("LocalReview performance guardrails failed:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return 1
    print("LocalReview release performance guardrails passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

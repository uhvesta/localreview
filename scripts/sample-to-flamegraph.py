#!/usr/bin/env python3
"""Render macOS `sample` call-graph output as a self-contained flamegraph SVG.

This deliberately consumes only the Call graph section and derives leaf/self
sample counts from inclusive node counts. It is a deterministic fallback for
machines where DTrace-based cargo-flamegraph is unavailable.
"""

from __future__ import annotations

import html
import re
import sys
from collections import defaultdict
from pathlib import Path


def folded_stacks(sample_text: str) -> dict[tuple[str, ...], int]:
    graph = sample_text.partition("Call graph:")[2].partition(
        "Total number in stack"
    )[0]
    nodes: list[dict[str, object]] = []
    roots: list[int] = []
    levels: list[tuple[int, int]] = []
    line_pattern = re.compile(r"^(\s+)(\d+)\s+(.+)$")
    for line in graph.splitlines():
        match = line_pattern.match(line)
        if not match:
            continue
        indent = len(match.group(1))
        count = int(match.group(2))
        name = re.sub(r"\s+\[[^\]]+\]$", "", match.group(3)).strip()
        name = re.sub(r"\s+\(in [^)]+\)(?:\s+\+\s+\d+)?$", "", name).strip()
        while levels and levels[-1][0] >= indent:
            levels.pop()
        parent = levels[-1][1] if levels else None
        index = len(nodes)
        nodes.append({"name": name, "count": count, "parent": parent, "children": []})
        if parent is None:
            roots.append(index)
        else:
            nodes[parent]["children"].append(index)  # type: ignore[index]
        levels.append((indent, index))

    folded: dict[tuple[str, ...], int] = defaultdict(int)

    def visit(index: int, stack: tuple[str, ...]) -> None:
        node = nodes[index]
        path = stack + (str(node["name"]),)
        children = list(node["children"])  # type: ignore[arg-type]
        child_count = sum(int(nodes[child]["count"]) for child in children)
        self_count = max(0, int(node["count"]) - child_count)
        if self_count:
            folded[path] += self_count
        if not children:
            folded[path] += int(node["count"])
            return
        for child in children:
            visit(child, path)

    for root in roots:
        visit(root, ())
    return dict(folded)


def color(name: str) -> str:
    value = 0
    for byte in name.encode("utf-8"):
        value = (value * 33 + byte) & 0xFFFFFFFF
    red = 205 + value % 40
    green = 80 + (value >> 8) % 115
    blue = 45 + (value >> 16) % 55
    return f"rgb({red},{green},{blue})"


def render(stacks: dict[tuple[str, ...], int], title: str) -> str:
    width = 1440
    frame_height = 18
    margin = 12
    header = 44
    total = max(1, sum(stacks.values()))
    max_depth = max((len(stack) for stack in stacks), default=1)
    height = header + max_depth * frame_height + margin * 2
    parts = [
        '<?xml version="1.0" encoding="UTF-8" standalone="no"?>',
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">',
        "<style>text{font-family:Menlo,monospace;font-size:11px;fill:#111}.title{font-size:16px;font-weight:bold}</style>",
        f'<rect width="{width}" height="{height}" fill="#fafafa"/>',
        f'<text class="title" x="{margin}" y="24">{html.escape(title)}</text>',
        f'<text x="{margin}" y="40">{total} sampled leaf/self frames</text>',
    ]
    x = float(margin)
    usable = width - margin * 2
    for stack, count in sorted(stacks.items(), key=lambda item: item[0]):
        stack_width = usable * count / total
        for depth, name in enumerate(stack):
            y = height - margin - (depth + 1) * frame_height
            parts.append(
                f'<g><title>{html.escape(name)} — {count} samples</title>'
                f'<rect x="{x:.3f}" y="{y}" width="{max(0.2, stack_width):.3f}" height="{frame_height - 1}" '
                f'fill="{color(name)}" stroke="#fff" stroke-width=".4"/>'
                f'<text x="{x + 3:.3f}" y="{y + 13}">{html.escape(name[:80])}</text></g>'
            )
        x += stack_width
    parts.append("</svg>")
    return "\n".join(parts)


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: sample-to-flamegraph.py STACKS.OUT FLAMEGRAPH.SVG", file=sys.stderr)
        return 2
    source, destination = map(Path, sys.argv[1:])
    stacks = folded_stacks(source.read_text(errors="replace"))
    destination.write_text(render(stacks, f"LocalReview release profile — {source.name}"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Gate Criterion results measured as a same-runner base/candidate pair."""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path


def load_results(path: Path) -> dict[str, tuple[float, str]]:
    """Load github-action-benchmark JSON keyed by benchmark identity."""
    payload = json.loads(path.read_text(encoding="utf-8"))
    results: dict[str, tuple[float, str]] = {}
    for item in payload:
        name = str(item["name"])
        if name in results:
            raise ValueError(f"duplicate benchmark {name!r} in {path}")
        results[name] = (float(item["value"]), str(item["unit"]))
    if not results:
        raise ValueError(f"no benchmark results in {path}")
    return results


def compare_results(
    base: dict[str, tuple[float, str]],
    candidate: dict[str, tuple[float, str]],
) -> list[tuple[str, float, float, float, str]]:
    """Return name, base, candidate, ratio, and unit for matching results."""
    if base.keys() != candidate.keys():
        missing = sorted(base.keys() - candidate.keys())
        added = sorted(candidate.keys() - base.keys())
        raise ValueError(f"benchmark set changed; missing={missing}, added={added}")

    comparisons = []
    for name in sorted(base):
        base_value, base_unit = base[name]
        candidate_value, candidate_unit = candidate[name]
        if base_unit != candidate_unit:
            raise ValueError(
                f"unit changed for {name!r}: {base_unit!r} -> {candidate_unit!r}"
            )
        if base_value <= 0:
            raise ValueError(f"base value for {name!r} must be positive")
        comparisons.append(
            (name, base_value, candidate_value, candidate_value / base_value, base_unit)
        )
    return comparisons


def render_summary(
    comparisons: list[tuple[str, float, float, float, str]],
    alert_ratio: float,
) -> str:
    """Render the paired comparison as a GitHub step summary."""
    lines = [
        "## Same-runner benchmark comparison",
        "",
        "| Benchmark | Base | Candidate | Ratio |",
        "|---|---:|---:|---:|",
    ]
    for name, base, candidate, ratio, unit in comparisons:
        marker = " **⚠**" if ratio > alert_ratio else ""
        lines.append(
            f"| `{name}` | {base:.6g} {unit} | {candidate:.6g} {unit} "
            f"| {ratio:.2f}×{marker} |"
        )
    return "\n".join(lines) + "\n"


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--alert-ratio", type=float, default=1.5)
    parser.add_argument("--fail-ratio", type=float, default=2.0)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    if not 1.0 <= args.alert_ratio <= args.fail_ratio:
        print("ratios must satisfy 1.0 <= alert <= fail", file=sys.stderr)
        return 2

    try:
        comparisons = compare_results(
            load_results(args.base), load_results(args.candidate)
        )
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        print(f"invalid benchmark comparison: {error}", file=sys.stderr)
        return 2

    summary = render_summary(comparisons, args.alert_ratio)
    print(summary, end="")
    if summary_path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(summary_path).open("a", encoding="utf-8") as summary_file:
            summary_file.write(summary)

    failures = [row for row in comparisons if row[3] > args.fail_ratio]
    if failures:
        print(
            f"{len(failures)} benchmark(s) exceeded the {args.fail_ratio:.2f}x "
            "same-runner failure threshold",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

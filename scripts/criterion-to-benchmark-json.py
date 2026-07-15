#!/usr/bin/env python3
"""Convert Criterion estimate files to github-action-benchmark JSON."""

from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) != 3:
        print(
            "usage: criterion-to-benchmark-json.py CRITERION_DIR OUTPUT.json",
            file=sys.stderr,
        )
        return 2

    root = Path(sys.argv[1])
    output = Path(sys.argv[2])
    results: list[dict[str, object]] = []
    for estimate_path in sorted(root.glob("**/new/estimates.json")):
        relative = estimate_path.relative_to(root)
        # Drop the trailing new/estimates.json components. The remaining path
        # is Criterion's stable group/function/parameter benchmark identity.
        name = "/".join(relative.parts[:-2])
        estimates = json.loads(estimate_path.read_text(encoding="utf-8"))
        mean_ns = float(estimates["mean"]["point_estimate"])
        deviation_ns = float(estimates["std_dev"]["point_estimate"])
        results.append(
            {
                "name": name,
                "unit": "ms",
                "value": mean_ns / 1_000_000,
                "range": str(deviation_ns / 1_000_000),
            }
        )

    if not results:
        print(f"no Criterion estimates found under {root}", file=sys.stderr)
        return 1
    output.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {len(results)} benchmark results to {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

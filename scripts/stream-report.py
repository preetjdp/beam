#!/usr/bin/env python3
"""Generate an attributable Beam streaming comparison from JSONL samples.

Each input line is one run with at least:
  {"treatment_id":"baseline", "workload_id":"scroll", "metric":"frame_age_ms",
   "value":42.1, "run_id":"1", "lower_is_better":true}

The tool deliberately refuses cells where manifest fingerprints differ within a
(treatment, workload) cell, preventing accidental multi-variable comparisons.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import math
import random
import statistics
from collections import defaultdict
from pathlib import Path


def percentile(values: list[float], q: float) -> float:
    ordered = sorted(values)
    if not ordered:
        return math.nan
    index = (len(ordered) - 1) * q
    lo, hi = math.floor(index), math.ceil(index)
    if lo == hi:
        return ordered[lo]
    return ordered[lo] * (hi - index) + ordered[hi] * (index - lo)


def bootstrap_delta(a: list[float], b: list[float], seed: int, rounds: int = 5000) -> tuple[float, float]:
    rng = random.Random(seed)
    deltas = []
    for _ in range(rounds):
        ma = statistics.median(rng.choices(a, k=len(a)))
        mb = statistics.median(rng.choices(b, k=len(b)))
        deltas.append(mb - ma)
    return percentile(deltas, 0.025), percentile(deltas, 0.975)


def fingerprint(row: dict) -> str:
    manifest = row.get("manifest", {})
    allowed = set(row.get("allowed_variables", []))
    canonical = {k: v for k, v in manifest.items() if k not in allowed}
    encoded = json.dumps(canonical, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()[:12]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("input", type=Path)
    parser.add_argument("--baseline", required=True)
    parser.add_argument("--markdown", type=Path, required=True)
    parser.add_argument("--csv", type=Path, required=True)
    args = parser.parse_args()

    cells: dict[tuple[str, str, str], list[dict]] = defaultdict(list)
    with args.input.open(encoding="utf-8") as source:
        for line_no, line in enumerate(source, 1):
            if not line.strip():
                continue
            row = json.loads(line)
            try:
                row["value"] = float(row["value"])
                key = (row["treatment_id"], row["workload_id"], row["metric"])
            except (KeyError, TypeError, ValueError) as exc:
                raise SystemExit(f"invalid sample at line {line_no}: {exc}") from exc
            if math.isfinite(row["value"]):
                cells[key].append(row)

    summaries = []
    for key, rows in sorted(cells.items()):
        prints = {fingerprint(row) for row in rows}
        if len(prints) != 1:
            raise SystemExit(f"mixed manifests in cell {key}: {sorted(prints)}")
        values = [row["value"] for row in rows]
        summaries.append({
            "treatment": key[0], "workload": key[1], "metric": key[2],
            "runs": len(values), "median": statistics.median(values),
            "p95": percentile(values, .95), "p99": percentile(values, .99),
            "fingerprint": next(iter(prints)), "values": values,
        })

    baseline = {(r["workload"], r["metric"]): r for r in summaries if r["treatment"] == args.baseline}
    for row in summaries:
        base = baseline.get((row["workload"], row["metric"]))
        if base and row["treatment"] != args.baseline:
            row["effect"] = row["median"] - base["median"]
            seed = int(hashlib.sha256(repr((row["treatment"], row["workload"], row["metric"])).encode()).hexdigest()[:8], 16)
            row["ci_low"], row["ci_high"] = bootstrap_delta(base["values"], row["values"], seed)

    args.csv.parent.mkdir(parents=True, exist_ok=True)
    with args.csv.open("w", newline="", encoding="utf-8") as output:
        writer = csv.DictWriter(output, fieldnames=["treatment", "workload", "metric", "runs", "median", "p95", "p99", "effect", "ci_low", "ci_high", "fingerprint"])
        writer.writeheader()
        for row in summaries:
            writer.writerow({k: row.get(k, "") for k in writer.fieldnames})

    lines = ["# Beam streaming experiment report", "", f"Baseline: `{args.baseline}`", "", "| Treatment | Workload | Metric | n | Median | p95 | p99 | Effect vs baseline (95% CI) | Manifest |", "| --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |"]
    for row in summaries:
        effect = "—" if "effect" not in row else f"{row['effect']:+.3f} [{row['ci_low']:+.3f}, {row['ci_high']:+.3f}]"
        lines.append(f"| {row['treatment']} | {row['workload']} | {row['metric']} | {row['runs']} | {row['median']:.3f} | {row['p95']:.3f} | {row['p99']:.3f} | {effect} | `{row['fingerprint']}` |")
    args.markdown.parent.mkdir(parents=True, exist_ok=True)
    args.markdown.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

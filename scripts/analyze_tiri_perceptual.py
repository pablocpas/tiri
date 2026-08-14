#!/usr/bin/env python3
"""Analyze and compare input/commit-to-vblank latency messages exported from Tracy."""

from __future__ import annotations

import argparse
import csv
import json
import math
import statistics
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any


LATENCY_FIELDS = (
    "input_ms",
    "commit_ms",
    "queue_ms",
    "render_ms",
    "submit_ms",
    "late_ms",
    "refresh_ms",
)
PHYSICAL_SOURCES = {
    "keyboard",
    "pointer-motion",
    "pointer-button",
    "pointer-axis",
    "touch",
    "gesture",
    "tablet",
}


class AnalysisError(RuntimeError):
    pass


def percentile(values: list[float], ratio: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    rank = (len(ordered) - 1) * ratio
    lower = math.floor(rank)
    upper = math.ceil(rank)
    if lower == upper:
        return ordered[lower]
    fraction = rank - lower
    return ordered[lower] + (ordered[upper] - ordered[lower]) * fraction


def parse_optional_float(value: str) -> float | None:
    if value == "-":
        return None
    return float(value)


def parse_message(message: str, timestamp_ns: int) -> dict[str, Any] | None:
    if not message.startswith("latency.present "):
        return None

    fields: dict[str, str] = {}
    for item in message.removeprefix("latency.present ").split():
        key, separator, value = item.partition("=")
        if not separator:
            continue
        fields[key] = value

    required = {"id", "backend", "output", "source", "sequence", *LATENCY_FIELDS}
    missing = required - fields.keys()
    if missing:
        raise AnalysisError(f"latency message is missing {sorted(missing)}: {message}")

    record: dict[str, Any] = {
        "timestamp_ns": timestamp_ns,
        "id": int(fields["id"]),
        "backend": fields["backend"],
        "output": fields["output"],
        "source": fields["source"],
        "sequence": int(fields["sequence"]),
    }
    for field in LATENCY_FIELDS:
        record[field] = parse_optional_float(fields[field])

    refresh_ms = record["refresh_ms"]
    input_ms = record["input_ms"]
    record["input_frames"] = (
        input_ms / refresh_ms
        if input_ms is not None and refresh_ms is not None and refresh_ms > 0
        else None
    )
    late_ms = record["late_ms"]
    record["missed_deadline"] = bool(
        refresh_ms is not None
        and refresh_ms > 0
        and late_ms is not None
        and late_ms > max(0.5, refresh_ms * 0.5)
    )
    return record


def read_records(directory: Path) -> tuple[list[dict[str, Any]], int]:
    path = directory / "messages.csv"
    records = []
    no_damage = 0
    try:
        with path.open(encoding="utf-8", newline="") as handle:
            for row in csv.DictReader(handle):
                message = row["MessageName"]
                if message.startswith("latency.no_damage "):
                    no_damage += 1
                    continue
                if not message.startswith("latency.present "):
                    continue
                record = parse_message(message, int(row["total_ns"]))
                if record is not None:
                    records.append(record)
    except FileNotFoundError as error:
        raise AnalysisError(f"missing Tracy message export: {path}") from error
    except (KeyError, ValueError) as error:
        raise AnalysisError(f"invalid Tracy message export {path}: {error}") from error
    return records, no_damage


def metric_summary(values: list[float]) -> dict[str, float | int]:
    return {
        "samples": len(values),
        "mean": statistics.fmean(values) if values else 0.0,
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
        "p99": percentile(values, 0.99),
        "max": max(values, default=0.0),
    }


def summarize_group(records: list[dict[str, Any]]) -> dict[str, Any]:
    metrics = {}
    for field in (*LATENCY_FIELDS, "input_frames"):
        values = [record[field] for record in records if record[field] is not None]
        metrics[field] = metric_summary(values)

    missed = sum(record["missed_deadline"] for record in records)
    return {
        "samples": len(records),
        "missed_deadlines": missed,
        "missed_deadline_rate": missed / len(records) if records else 0.0,
        "metrics": metrics,
    }


def collapse_by_trigger(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Keep the earliest visible presentation for a trigger queued on multiple outputs."""
    selected: dict[int, dict[str, Any]] = {}
    for record in records:
        current = selected.get(record["id"])
        if current is None or record["timestamp_ns"] < current["timestamp_ns"]:
            selected[record["id"]] = record
    return sorted(selected.values(), key=lambda record: record["timestamp_ns"])


def analyze(directory: Path) -> dict[str, Any]:
    directory = directory.resolve()
    records, no_damage = read_records(directory)
    if not records:
        raise AnalysisError(
            f"no presented latency samples in {directory}; use a Tracy-enabled tiri on the DRM "
            "backend and interact with a visible surface"
        )

    unique_records = collapse_by_trigger(records)
    by_source: dict[str, list[dict[str, Any]]] = defaultdict(list)
    by_output: dict[str, list[dict[str, Any]]] = defaultdict(list)
    physical = []
    for record in unique_records:
        by_source[record["source"]].append(record)
        if record["source"] in PHYSICAL_SOURCES:
            physical.append(record)
    for record in records:
        by_output[record["output"]].append(record)

    return {
        "format": 2,
        "directory": str(directory),
        "backends": sorted({record["backend"] for record in records}),
        "outputs": sorted(by_output),
        "presented_samples": len(records),
        "unique_triggers": len(unique_records),
        "no_damage_samples": no_damage,
        "overall": summarize_group(unique_records),
        "all_output_presentations": summarize_group(records),
        "physical_input": summarize_group(physical),
        "by_source": {
            name: summarize_group(group) for name, group in sorted(by_source.items())
        },
        "by_output": {
            name: summarize_group(group) for name, group in sorted(by_output.items())
        },
    }


def print_group(label: str, group: dict[str, Any]) -> None:
    input_metric = group["metrics"]["input_ms"]
    commit_metric = group["metrics"]["commit_ms"]
    late_metric = group["metrics"]["late_ms"]
    print(
        f"{label:24} samples={group['samples']:5d} "
        f"input p50/p95/p99={input_metric['p50']:.2f}/{input_metric['p95']:.2f}/"
        f"{input_metric['p99']:.2f} ms "
        f"commit p95={commit_metric['p95']:.2f} ms "
        f"late p95={late_metric['p95']:.2f} ms "
        f"missed={group['missed_deadline_rate'] * 100:.2f}%"
    )


def print_analysis(report: dict[str, Any]) -> None:
    print(report["directory"])
    print(f"backends={','.join(report['backends'])} outputs={','.join(report['outputs'])}")
    print_group("overall", report["overall"])
    print_group("physical-input", report["physical_input"])
    for name, group in report["by_source"].items():
        print_group(f"source:{name}", group)


def percent_delta(baseline: float, candidate: float) -> float:
    if baseline == 0:
        return float("inf") if candidate > 0 else 0.0
    return (candidate - baseline) / baseline * 100.0


def compare_reports(args: argparse.Namespace) -> int:
    baseline = json.loads(args.baseline.read_text(encoding="utf-8"))
    candidate = json.loads(args.candidate.read_text(encoding="utf-8"))
    regressions = []
    comparisons = []

    base_sources = baseline.get("by_source", {})
    cand_sources = candidate.get("by_source", {})
    for source in sorted(set(base_sources) & set(cand_sources)):
        base = base_sources[source]
        cand = cand_sources[source]
        if min(base["samples"], cand["samples"]) < args.min_samples:
            continue
        for metric_name in ("input_ms", "commit_ms"):
            base_metric = base["metrics"][metric_name]
            cand_metric = cand["metrics"][metric_name]
            if min(base_metric["samples"], cand_metric["samples"]) < args.min_samples:
                continue
            for percentile_name in ("p50", "p95", "p99"):
                base_value = float(base_metric[percentile_name])
                cand_value = float(cand_metric[percentile_name])
                delta_ms = cand_value - base_value
                delta_pct = percent_delta(base_value, cand_value)
                comparisons.append(
                    (
                        source,
                        metric_name,
                        percentile_name,
                        base_value,
                        cand_value,
                        delta_ms,
                        delta_pct,
                    )
                )
                if (
                    percentile_name in ("p95", "p99")
                    and delta_ms >= args.latency_abs_ms
                    and delta_pct >= args.latency_pct
                ):
                    regressions.append(
                        f"{source}: {metric_name} {percentile_name} {base_value:.2f} -> "
                        f"{cand_value:.2f} ms (+{delta_ms:.2f} ms, +{delta_pct:.1f}%)"
                    )

        base_missed = float(base["missed_deadline_rate"])
        cand_missed = float(cand["missed_deadline_rate"])
        if cand_missed - base_missed >= args.missed_abs_rate:
            regressions.append(
                f"{source}: missed deadline rate {base_missed * 100:.2f}% -> "
                f"{cand_missed * 100:.2f}%"
            )

    if comparisons:
        print("Perceptual latency deltas (negative is faster):")
        for source, metric, percentile_name, base, cand, delta_ms, delta_pct in comparisons:
            print(
                f"  {source:16} {metric:10} {percentile_name}: "
                f"{base:8.2f} -> {cand:8.2f} ms  "
                f"{delta_ms:+8.2f} ms  {delta_pct:+7.1f}%"
            )
    else:
        print(
            "No common source/metric had enough samples for a numerical comparison "
            f"(minimum {args.min_samples})."
        )

    if regressions:
        print("Perceptual latency regressions detected:")
        for regression in regressions:
            print(f"  - {regression}")
        return 1
    print("No perceptual latency regression exceeded the configured thresholds.")
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    analyze_parser = subparsers.add_parser("analyze")
    analyze_parser.add_argument("directories", nargs="+", type=Path)
    analyze_parser.add_argument("--output", type=Path)

    compare_parser = subparsers.add_parser("compare")
    compare_parser.add_argument("--baseline", type=Path, required=True)
    compare_parser.add_argument("--candidate", type=Path, required=True)
    compare_parser.add_argument("--min-samples", type=int, default=30)
    compare_parser.add_argument("--latency-pct", type=float, default=10.0)
    compare_parser.add_argument("--latency-abs-ms", type=float, default=2.0)
    compare_parser.add_argument("--missed-abs-rate", type=float, default=0.01)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.command == "compare":
            return compare_reports(args)

        reports = [analyze(directory) for directory in args.directories]
        for report in reports:
            print_analysis(report)
        if args.output is not None:
            value: Any = reports[0] if len(reports) == 1 else reports
            args.output.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
        return 0
    except (OSError, ValueError, KeyError, json.JSONDecodeError, AnalysisError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

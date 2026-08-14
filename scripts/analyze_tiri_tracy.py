#!/usr/bin/env python3
"""Summarize the Tracy CSV artifacts produced by profile_tiri_headless_tracy.py."""

from __future__ import annotations

import argparse
import csv
import json
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any


IMPORTANT_ZONES = (
    "State::refresh_and_flush_clients",
    "State::refresh",
    "Niri::refresh_window_states",
    "Layout::refresh",
    "Workspace::refresh",
    "TreeSpace::refresh",
    "FloatingSpace::refresh",
    "foreign_toplevel::refresh",
    "ext_workspace::refresh",
    "State::ipc_refresh_windows",
    "State::ipc_refresh_workspaces",
    "Niri::advance_animations",
    "Niri::redraw_queued_outputs",
    "Niri::redraw",
    "Layout::update_render_elements",
    "TreeSpace::update_render_elements",
    "FloatingSpace::update_render_elements",
    "TreeSpace::clone_state_layouts_for_refresh",
    "TreeSpace::clone_state_layouts_for_render",
    "TreeSpace::clone_display_layouts_for_render",
    "FloatingSpace::clone_display_layouts_for_render",
    "TreeSpace::project_state_layouts_for_refresh",
    "TreeSpace::project_state_layouts_for_render",
    "TreeSpace::project_display_layouts_for_render",
    "FloatingSpace::project_display_layouts_for_render",
    "Mapped::send_pending_configure",
    "Mapped::send_configure",
    "ContainerTree::debug_layout_state",
)


def read_zones(path: Path) -> tuple[dict[str, dict[str, Any]], list[dict[str, Any]]]:
    zones: dict[str, dict[str, Any]] = {}
    all_zones = []
    with path.open(encoding="utf-8", newline="") as handle:
        for row in csv.DictReader(handle):
            item = {
                "name": row["name"],
                "total_ms": int(row["total_ns"]) / 1_000_000.0,
                "calls": int(row["counts"]),
                "mean_us": float(row["mean_ns"]) / 1_000.0,
                "min_us": float(row["min_ns"]) / 1_000.0,
                "max_us": float(row["max_ns"]) / 1_000.0,
            }
            all_zones.append(item)
            if item["name"] in IMPORTANT_ZONES:
                zones[item["name"]] = item
    all_zones.sort(key=lambda item: item["total_ms"], reverse=True)
    return zones, all_zones


def read_plots(path: Path) -> dict[str, dict[str, float | int]]:
    values: dict[str, list[float]] = defaultdict(list)
    with path.open(encoding="utf-8", newline="") as handle:
        for row in csv.DictReader(handle):
            name = row["name"]
            if not (
                name.startswith(("layout.", "redraw.", "refresh.", "latency."))
                or "frame schedule" in name
                or "predicted render time" in name
            ):
                continue
            try:
                values[name].append(float(row["value"]))
            except ValueError:
                continue
    return {
        name: {
            "events": len(series),
            "sum": sum(series),
            "mean": sum(series) / len(series),
            "max": max(series),
        }
        for name, series in sorted(values.items())
        if series
    }


def analyze(directory: Path) -> dict[str, Any]:
    metadata_path = directory / "metadata.json"
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    zones, all_zones = read_zones(directory / "cpu-zones-summary.csv")
    return {
        "directory": str(directory),
        "metadata": metadata,
        "zones": zones,
        "plots": read_plots(directory / "plots.csv"),
        "top_zones": all_zones[:30],
    }


def print_analysis(analyses: list[dict[str, Any]]) -> None:
    for analysis in analyses:
        metadata = analysis["metadata"]
        print(
            f"\n{analysis['directory']} "
            f"outputs={metadata['outputs']} initial={metadata.get('initial_windows_override')}"
        )
        print(f"{'zone':58} {'total ms':>10} {'calls':>9} {'mean us':>10}")
        for name in IMPORTANT_ZONES:
            zone = analysis["zones"].get(name)
            if zone is None:
                continue
            print(
                f"{name[:58]:58} {zone['total_ms']:10.3f} "
                f"{zone['calls']:9d} {zone['mean_us']:10.3f}"
            )
        print("plots:")
        for name, plot in analysis["plots"].items():
            print(
                f"  {name[:45]:45} events={plot['events']:6d} "
                f"sum={plot['sum']:10.3f} mean={plot['mean']:8.3f} max={plot['max']:8.3f}"
            )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directories", nargs="+", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    try:
        analyses = [analyze(path.resolve()) for path in args.directories]
    except (OSError, KeyError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    print_analysis(analyses)
    if args.output is not None:
        args.output.write_text(json.dumps(analyses, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Run and compare reproducible Tracy profiles for typical tiri workflows."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

sys.dont_write_bytecode = True

from analyze_tiri_tracy import analyze


@dataclass(frozen=True)
class Workflow:
    scenario: str
    window_command: str
    outputs: int = 1
    settle_timeout: float = 5.0


ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = {
    "open_close_foot": Workflow(
        "open_close.json",
        "foot --app-id=perf-open-close --title=tiri-open-close",
    ),
    "terminal_idle": Workflow(
        "terminal_idle.json",
        "foot --app-id=perf-terminal-idle --title=tiri-terminal-idle",
    ),
    "terminal_activity": Workflow(
        "terminal_activity.json",
        os.fspath(ROOT / "scripts/perf_workloads/run_terminal_activity.sh"),
    ),
    "browser_activity": Workflow(
        "browser_activity.json",
        os.fspath(ROOT / "scripts/perf_workloads/run_browser_activity.sh"),
        settle_timeout=10.0,
    ),
    "multi_output_foot": Workflow(
        "multi_output_workflow.json",
        "foot --app-id=perf-multi-output --title=tiri-multi-output",
        outputs=3,
    ),
}

COMPARE_ZONES = (
    "State::refresh_and_flush_clients",
    "State::refresh",
    "Niri::refresh_window_states",
    "Layout::refresh",
    "foreign_toplevel::refresh",
    "ext_workspace::refresh",
    "State::ipc_refresh_windows",
    "State::ipc_refresh_workspaces",
    "Niri::redraw",
    "Layout::update_render_elements",
)

SUMMARY_ZONES = (
    "State::refresh_and_flush_clients",
    "State::refresh",
    "Layout::refresh",
    "Niri::redraw",
)

# These zones do approximately the same work per invocation. Refresh zones are
# deliberately absent: after incremental refresh their remaining calls are the
# expensive, dirty ones, so comparing their conditional mean is misleading.
PER_CALL_ZONES = (
    "Niri::redraw",
    "Layout::update_render_elements",
)


def selected_workflows(names: list[str] | None) -> list[tuple[str, Workflow]]:
    if not names:
        return list(WORKFLOWS.items())
    unknown = sorted(set(names) - WORKFLOWS.keys())
    if unknown:
        raise ValueError(f"unknown workflows: {', '.join(unknown)}")
    return [(name, WORKFLOWS[name]) for name in names]


def run_workflows(args: argparse.Namespace) -> int:
    output_dir = args.output_dir.resolve()
    if output_dir.exists() and any(output_dir.iterdir()):
        raise ValueError(f"output directory is not empty: {output_dir}")
    output_dir.mkdir(parents=True, exist_ok=True)

    report: dict[str, Any] = {
        "format": 1,
        "tiri": os.fspath(args.tiri.resolve()),
        "repeat": args.repeat,
        "workflows": {},
    }
    for name, workflow in selected_workflows(args.workflow):
        workflow_dir = output_dir / name
        command = [
            os.fspath(ROOT / "scripts/profile_tiri_headless_tracy.py"),
            "--tiri",
            os.fspath(args.tiri.resolve()),
            "--scenario",
            os.fspath(ROOT / "scripts/perf_scenarios" / workflow.scenario),
            "--output-dir",
            os.fspath(workflow_dir),
            "--window-cmd",
            workflow.window_command,
            "--repeat",
            str(args.repeat),
            "--outputs",
            str(workflow.outputs),
            "--settle-timeout",
            str(workflow.settle_timeout),
            "--ipc-timeout",
            str(args.ipc_timeout),
            "--rust-log",
            args.rust_log,
        ]
        print(f"\n== {name} ==", flush=True)
        subprocess.run(command, cwd=ROOT, check=True)

        analysis = analyze(workflow_dir)
        scenario_summary = json.loads(
            (workflow_dir / "scenario/summary.json").read_text(encoding="utf-8")
        )
        report["workflows"][name] = {
            "analysis": analysis,
            "steps": scenario_summary["step_summaries"],
        }

    report_path = output_dir / "workflows.json"
    report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"\n{report_path}")
    return 0


def percent_delta(baseline: float, candidate: float) -> float:
    if baseline == 0:
        return float("inf") if candidate > 0 else 0.0
    return (candidate - baseline) / baseline * 100.0


def compare_reports(args: argparse.Namespace) -> int:
    baseline = json.loads(args.baseline.read_text(encoding="utf-8"))
    candidate = json.loads(args.candidate.read_text(encoding="utf-8"))
    regressions: list[str] = []
    comparisons: list[tuple[str, str, float, float, int, int]] = []

    baseline_workflows = baseline.get("workflows", {})
    candidate_workflows = candidate.get("workflows", {})
    requested_workflows = set(args.workflow or ())
    if requested_workflows:
        missing_baseline = requested_workflows - set(baseline_workflows)
        missing_candidate = requested_workflows - set(candidate_workflows)
        if missing_baseline or missing_candidate:
            raise ValueError("selected workflows must exist in both reports")
        workflow_names = sorted(requested_workflows)
    else:
        if set(baseline_workflows) != set(candidate_workflows):
            raise ValueError("baseline and candidate must contain the same workflows")
        workflow_names = sorted(baseline_workflows)

    baseline_repeat = int(baseline.get("repeat", 0))
    candidate_repeat = int(candidate.get("repeat", 0))
    if baseline_repeat < 1 or candidate_repeat < 1:
        raise ValueError("reports must record a positive repeat count")
    if baseline_repeat != candidate_repeat:
        raise ValueError("baseline and candidate must use the same repeat count")

    for name in workflow_names:
        base = baseline_workflows[name]
        cand = candidate_workflows[name]
        base_zones = base["analysis"]["zones"]
        cand_zones = cand["analysis"]["zones"]
        for zone_name in COMPARE_ZONES:
            if zone_name not in base_zones or zone_name not in cand_zones:
                continue
            base_total = float(base_zones[zone_name]["total_ms"]) / baseline_repeat
            cand_total = float(cand_zones[zone_name]["total_ms"]) / candidate_repeat
            base_calls = int(base_zones[zone_name]["calls"])
            cand_calls = int(cand_zones[zone_name]["calls"])
            comparisons.append(
                (name, zone_name, base_total, cand_total, base_calls, cand_calls)
            )
            delta_ms = cand_total - base_total
            delta_pct = percent_delta(base_total, cand_total)
            if delta_ms >= args.zone_abs_ms and delta_pct >= args.zone_pct:
                regressions.append(
                    f"{name}: {zone_name} CPU/run {base_total:.2f} -> {cand_total:.2f} ms "
                    f"(+{delta_ms:.2f} ms, +{delta_pct:.1f}%)"
                )

            if zone_name in PER_CALL_ZONES:
                base_mean = float(base_zones[zone_name]["mean_us"])
                cand_mean = float(cand_zones[zone_name]["mean_us"])
                call_delta_pct = abs(percent_delta(base_calls, cand_calls))
                mean_delta_us = cand_mean - base_mean
                mean_delta_pct = percent_delta(base_mean, cand_mean)
                if (
                    call_delta_pct <= args.per_call_count_tolerance
                    and mean_delta_us >= args.per_call_abs_us
                    and mean_delta_pct >= args.per_call_pct
                ):
                    regressions.append(
                        f"{name}: {zone_name} mean/call {base_mean:.2f} -> "
                        f"{cand_mean:.2f} us (+{mean_delta_us:.2f} us, "
                        f"+{mean_delta_pct:.1f}%)"
                    )

        base_steps = {step["label"]: step for step in base.get("steps", [])}
        for step in cand.get("steps", []):
            base_step = base_steps.get(step["label"])
            if base_step is None:
                continue
            if (
                int(base_step.get("sample_count", 0)) < args.min_step_samples
                or int(step.get("sample_count", 0)) < args.min_step_samples
            ):
                continue
            base_p95 = float(base_step["p95_ms"])
            cand_p95 = float(step["p95_ms"])
            delta_ms = cand_p95 - base_p95
            delta_pct = percent_delta(base_p95, cand_p95)
            if delta_ms >= args.step_abs_ms and delta_pct >= args.step_pct:
                regressions.append(
                    f"{name}: step {step['label']} p95 {base_p95:.2f} -> {cand_p95:.2f} ms "
                    f"(+{delta_ms:.2f} ms, +{delta_pct:.1f}%)"
                )

        plots = cand["analysis"].get("plots", {})
        for invariant in (
            "refresh.configure_self_invalidation",
            "refresh.layout_self_invalidation",
        ):
            value = float(plots.get(invariant, {}).get("sum", 0.0))
            if value != 0:
                regressions.append(f"{name}: invariant {invariant} activated {value:.0f} times")

        base_global = float(
            base["analysis"].get("plots", {}).get("redraw.scope_all", {}).get("mean", 0.0)
        )
        cand_global = float(plots.get("redraw.scope_all", {}).get("mean", 0.0))
        if cand_global > base_global + args.global_redraw_rate:
            regressions.append(
                f"{name}: global redraw rate {base_global:.4f} -> {cand_global:.4f}"
            )

    print("CPU per run (total zone time; calls cover the complete capture):")
    print(f"{'workflow':22} {'zone':34} {'before ms':>10} {'after ms':>10} {'delta':>9} {'calls':>15}")
    for name, zone_name, base_total, cand_total, base_calls, cand_calls in comparisons:
        if zone_name not in SUMMARY_ZONES:
            continue
        delta_pct = percent_delta(base_total, cand_total)
        print(
            f"{name[:22]:22} {zone_name[:34]:34} {base_total:10.3f} "
            f"{cand_total:10.3f} {delta_pct:+8.1f}% "
            f"{base_calls:7d}->{cand_calls:<7d}"
        )

    if regressions:
        print()
        print("Performance regressions detected:")
        for regression in regressions:
            print(f"  - {regression}")
        return 1

    print()
    print("No performance regressions exceeded the configured thresholds.")
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    run = subparsers.add_parser("run", help="run the workflow matrix")
    run.add_argument("--tiri", type=Path, default=ROOT / "target/tracy/release/tiri")
    run.add_argument("--output-dir", type=Path, required=True)
    run.add_argument("--repeat", type=int, default=6)
    run.add_argument("--workflow", action="append", choices=sorted(WORKFLOWS))
    run.add_argument("--ipc-timeout", type=float, default=5.0)
    run.add_argument("--rust-log", default="warn")
    run.set_defaults(func=run_workflows)

    compare = subparsers.add_parser("compare", help="compare two workflow reports")
    compare.add_argument("--baseline", type=Path, required=True)
    compare.add_argument("--candidate", type=Path, required=True)
    compare.add_argument("--workflow", action="append", choices=sorted(WORKFLOWS))
    compare.add_argument("--zone-pct", type=float, default=15.0)
    compare.add_argument("--zone-abs-ms", type=float, default=5.0)
    compare.add_argument("--per-call-pct", type=float, default=25.0)
    compare.add_argument("--per-call-abs-us", type=float, default=20.0)
    compare.add_argument("--per-call-count-tolerance", type=float, default=10.0)
    compare.add_argument("--step-pct", type=float, default=20.0)
    compare.add_argument("--step-abs-ms", type=float, default=20.0)
    compare.add_argument("--min-step-samples", type=int, default=5)
    compare.add_argument("--global-redraw-rate", type=float, default=0.01)
    compare.set_defaults(func=compare_reports)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if getattr(args, "repeat", 1) < 1:
            raise ValueError("--repeat must be positive")
        if getattr(args, "ipc_timeout", 1.0) <= 0:
            raise ValueError("--ipc-timeout must be positive")
        return args.func(args)
    except (OSError, ValueError, subprocess.CalledProcessError, KeyError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""
Local profiling runner for tiri sessions.

This script drives a live tiri instance over IPC, runs a repeatable scenario,
and writes per-run plus aggregate timing summaries. It is meant to complement
Tracy: use the numeric summaries to compare runs and Tracy to inspect hotspots.
"""

from __future__ import annotations

import argparse
import csv
import json
import math
import os
import select
import signal
import socket
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


JsonValue = Any


class RunnerError(RuntimeError):
    pass


@dataclass(frozen=True)
class StepSpec:
    sequence: int
    kind: str
    label: str
    action_name: str | None = None
    action_args: dict[str, JsonValue] | None = None


@dataclass
class StepResult:
    sequence: int
    label: str
    kind: str
    action_name: str | None
    duration_ms: float
    total_windows: int
    workspace_windows: int
    focused_window_id: int | None


@dataclass(frozen=True)
class ActiveWorkspace:
    id: int
    name: str
    restore_name: str | None
    restore_reference: dict[str, JsonValue] | None
    renamed_from_focused_workspace: bool


@dataclass
class ScenarioSpec:
    name: str
    initial_windows: int
    steps: list[StepSpec]


class EventMonitor:
    def __init__(self, socket_path: Path) -> None:
        self._socket_path = socket_path
        self._sock: socket.socket | None = None
        self._file = None
        self._thread: threading.Thread | None = None
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._last_event_at = time.monotonic()
        self._error: Exception | None = None

    def start(self) -> None:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(os.fspath(self._socket_path))
        file = sock.makefile("rwb", buffering=0)
        file.write(b'"EventStream"\n')
        reply_line = file.readline()
        if not reply_line:
            raise RunnerError("event stream closed before replying")
        reply = json.loads(reply_line)
        if reply != {"Ok": "Handled"}:
            raise RunnerError(f"unexpected event stream reply: {reply!r}")

        self._sock = sock
        self._file = file
        self._thread = threading.Thread(target=self._run, name="tiri-event-stream", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._sock is not None:
            try:
                self._sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self._sock.close()
        if self._file is not None:
            try:
                self._file.close()
            except OSError:
                pass
        if self._thread is not None:
            self._thread.join(timeout=1.0)

    def reset_activity(self) -> None:
        with self._lock:
            self._last_event_at = time.monotonic()

    def quiet_for(self) -> float:
        with self._lock:
            return time.monotonic() - self._last_event_at

    def raise_if_failed(self) -> None:
        if self._error is not None:
            raise RunnerError(f"event stream failed: {self._error}") from self._error

    def _note_event(self) -> None:
        with self._lock:
            self._last_event_at = time.monotonic()

    def _run(self) -> None:
        assert self._sock is not None
        assert self._file is not None

        try:
            while not self._stop.is_set():
                ready, _, _ = select.select([self._sock], [], [], 0.1)
                if not ready:
                    continue
                line = self._file.readline()
                if not line:
                    if self._stop.is_set():
                        return
                    raise RunnerError("event stream closed unexpectedly")
                json.loads(line)
                self._note_event()
        except Exception as err:  # noqa: BLE001
            if not self._stop.is_set():
                self._error = err


class Runner:
    def __init__(self, args: argparse.Namespace, scenario: ScenarioSpec) -> None:
        self.args = args
        self.scenario = scenario
        self.socket_path = self._resolve_socket_path(args.socket)
        self.output_dir = args.output_dir.resolve()
        self.event_monitor = EventMonitor(self.socket_path)

    @staticmethod
    def _resolve_socket_path(explicit_socket: str | None) -> Path:
        value = explicit_socket or os.environ.get("TIRI_SOCKET")
        if not value:
            raise RunnerError("TIRI_SOCKET is not set; pass --socket or run this inside tiri")
        path = Path(value)
        if not path.exists():
            raise RunnerError(f"tiri socket does not exist: {path}")
        return path

    def run(self) -> int:
        self.output_dir.mkdir(parents=True, exist_ok=True)

        self.event_monitor.start()
        try:
            self._wait_for_quiet("before first run")
            baseline_workspace = self._focused_workspace()
            baseline_reference = workspace_reference(baseline_workspace)

            runs = []
            for run_index in range(self.args.repeat):
                runs.append(self._run_once(run_index, baseline_reference))

            summary = self._build_summary(runs)
            self._write_summary(summary)
            self._print_summary(summary)
            return 0
        finally:
            self.event_monitor.stop()

    def _run_once(
        self, run_index: int, baseline_reference: dict[str, JsonValue] | None
    ) -> dict[str, JsonValue]:
        workspace_name = build_workspace_name(
            prefix=self.args.workspace_prefix,
            scenario_name=self.scenario.name,
            run_index=run_index,
        )
        spawned_processes: list[subprocess.Popen[str]] = []
        step_results: list[StepResult] = []
        scenario_start = time.perf_counter()
        active_workspace: ActiveWorkspace | None = None

        try:
            active_workspace = self._activate_workspace(workspace_name, spawned_processes)
            workspace_id = active_workspace.id
            snapshot = self._settle(f"workspace {workspace_name} baseline")

            sequence = 0
            for idx in range(self.scenario.initial_windows):
                label = f"initial_window_{idx + 1}"
                step = StepSpec(sequence=sequence, kind="spawn_window", label=label)
                result, snapshot = self._run_step(
                    step=step,
                    workspace_id=workspace_id,
                    previous_snapshot=snapshot,
                    spawned_processes=spawned_processes,
                )
                step_results.append(result)
                sequence += 1

            for step in self.scenario.steps:
                adjusted_step = StepSpec(
                    sequence=sequence,
                    kind=step.kind,
                    label=step.label,
                    action_name=step.action_name,
                    action_args=step.action_args,
                )
                result, snapshot = self._run_step(
                    step=adjusted_step,
                    workspace_id=workspace_id,
                    previous_snapshot=snapshot,
                    spawned_processes=spawned_processes,
                )
                step_results.append(result)
                sequence += 1

            total_duration_ms = (time.perf_counter() - scenario_start) * 1000.0
            return {
                "run_index": run_index,
                "warmup": run_index == 0,
                "workspace_name": active_workspace.name,
                "workspace_id": workspace_id,
                "total_duration_ms": total_duration_ms,
                "steps": [step_result_to_json(step) for step in step_results],
            }
        finally:
            if self.args.cleanup and active_workspace is not None:
                self._cleanup_workspace(active_workspace)
            self._terminate_processes(spawned_processes)
            if baseline_reference is not None:
                try:
                    self._focus_workspace_reference(baseline_reference)
                    self._wait_for_quiet("baseline workspace restore")
                except RunnerError:
                    pass

    def _run_step(
        self,
        step: StepSpec,
        workspace_id: int,
        previous_snapshot: dict[str, JsonValue],
        spawned_processes: list[subprocess.Popen[str]],
    ) -> tuple[StepResult, dict[str, JsonValue]]:
        self.event_monitor.raise_if_failed()
        self.event_monitor.reset_activity()
        start = time.perf_counter()
        before_ids = {
            window["id"]
            for window in previous_snapshot.get("windows", [])
            if isinstance(window.get("id"), int)
        }

        if step.kind == "spawn_window":
            spawned_processes.append(self._spawn_window())
            self._wait_for_new_window_id(
                before_ids,
                context=f"{step.label} window",
            )
        elif step.kind == "action":
            assert step.action_name is not None
            self._send_action(step.action_name, step.action_args)
        else:
            raise RunnerError(f"unsupported step kind: {step.kind}")

        snapshot = self._settle(step.label)
        duration_ms = (time.perf_counter() - start) * 1000.0

        if step.kind == "spawn_window":
            before = previous_snapshot["workspace_window_count"]
            after = snapshot["workspace_window_count"]
            if after <= before:
                raise RunnerError(
                    f"{step.label} did not increase workspace window count ({before} -> {after})"
                )

        result = StepResult(
            sequence=step.sequence,
            label=step.label,
            kind=step.kind,
            action_name=step.action_name,
            duration_ms=duration_ms,
            total_windows=snapshot["window_count"],
            workspace_windows=snapshot["workspace_window_count"],
            focused_window_id=snapshot["focused_window_id"],
        )
        return result, snapshot

    def _settle(self, context: str) -> dict[str, JsonValue]:
        deadline = time.monotonic() + self.args.settle_timeout
        previous_key: str | None = None
        stable_matches = 0
        latest_snapshot: dict[str, JsonValue] | None = None

        while time.monotonic() < deadline:
            self.event_monitor.raise_if_failed()
            if self.event_monitor.quiet_for() < self.args.idle_grace:
                time.sleep(self.args.settle_interval)
                continue

            latest_snapshot = self._capture_snapshot()
            snapshot_key = json.dumps(
                latest_snapshot,
                sort_keys=True,
                separators=(",", ":"),
            )

            if snapshot_key == previous_key:
                stable_matches += 1
                if stable_matches >= 2:
                    return latest_snapshot
            else:
                previous_key = snapshot_key
                stable_matches = 1

            time.sleep(self.args.settle_interval)

        if latest_snapshot is None:
            latest_snapshot = self._capture_snapshot()
        raise RunnerError(
            f"timed out waiting for settle after {context}; "
            f"latest snapshot: {json.dumps(latest_snapshot, sort_keys=True)}"
        )

    def _wait_for_quiet(self, context: str) -> None:
        deadline = time.monotonic() + self.args.settle_timeout
        while time.monotonic() < deadline:
            self.event_monitor.raise_if_failed()
            if self.event_monitor.quiet_for() >= self.args.idle_grace:
                return
            time.sleep(self.args.settle_interval)
        raise RunnerError(f"timed out waiting for quiet period: {context}")

    def _capture_snapshot(self) -> dict[str, JsonValue]:
        windows = self._request_variant("Windows", "Windows")
        layout_tree = self._request_variant("LayoutTree", "LayoutTree")
        workspaces = self._request_variant("Workspaces", "Workspaces")
        focused_workspace = next((ws for ws in workspaces if ws.get("is_focused")), None)
        workspace_id = focused_workspace.get("id") if focused_workspace else None
        normalized = normalize_snapshot(windows, layout_tree, workspace_id)
        return normalized

    def _request(self, request: JsonValue) -> JsonValue:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.connect(os.fspath(self.socket_path))
            file = sock.makefile("rwb", buffering=0)
            payload = json.dumps(request, separators=(",", ":")).encode("utf-8")
            file.write(payload + b"\n")
            reply_line = file.readline()
            if not reply_line:
                raise RunnerError(f"empty reply for request {request!r}")
        reply = json.loads(reply_line)
        if not isinstance(reply, dict) or len(reply) != 1:
            raise RunnerError(f"unexpected reply shape: {reply!r}")
        key, value = next(iter(reply.items()))
        if key == "Err":
            raise RunnerError(f"{value}; request={json.dumps(request, sort_keys=True)}")
        if key != "Ok":
            raise RunnerError(f"unexpected reply variant: {reply!r}")
        return value

    def _request_variant(self, request: JsonValue, variant: str) -> JsonValue:
        response = self._request(request)
        if variant == "Handled":
            if response != "Handled":
                raise RunnerError(f"expected Handled, got {response!r}")
            return response
        if not isinstance(response, dict) or variant not in response:
            raise RunnerError(f"expected {variant}, got {response!r}")
        return response[variant]

    def _send_action(self, action_name: str, action_args: dict[str, JsonValue] | None) -> None:
        payload = {} if action_args is None else strip_nulls(action_args)
        action: JsonValue = {action_name: payload}
        try:
            self._request_variant({"Action": action}, "Handled")
        except RunnerError as err:
            raise RunnerError(f"action {action_name} failed: {err}") from err

    def _spawn_window(self) -> subprocess.Popen[str]:
        env = os.environ.copy()
        env["TIRI_SOCKET"] = os.fspath(self.socket_path)
        return subprocess.Popen(
            self.args.window_cmd,
            shell=True,
            executable="/bin/bash",
            env=env,
            start_new_session=True,
            text=True,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def _focused_workspace(self, expected_name: str | None = None) -> dict[str, JsonValue]:
        workspaces = self._request_variant("Workspaces", "Workspaces")
        focused = next((ws for ws in workspaces if ws.get("is_focused")), None)
        if focused is None:
            raise RunnerError("no focused workspace reported by IPC")
        if expected_name is not None and focused.get("name") != expected_name:
            raise RunnerError(
                f"expected focused workspace {expected_name!r}, got {focused.get('name')!r}"
            )
        return focused

    def _focus_workspace_by_name(self, name: str) -> None:
        self._focus_workspace_reference({"Name": name})

    def _focus_workspace_reference(self, reference: dict[str, JsonValue]) -> None:
        self._send_action("FocusWorkspace", {"reference": reference})

    def _activate_workspace(
        self,
        workspace_name: str,
        spawned_processes: list[subprocess.Popen[str]],
    ) -> ActiveWorkspace:
        focused_before = self._focused_workspace()
        restore_reference = workspace_reference(focused_before)
        restore_name = focused_before.get("name")
        if not isinstance(restore_name, str):
            restore_name = None

        self._focus_workspace_by_name(workspace_name)
        self._wait_for_quiet(f"workspace {workspace_name} activation")

        focused = self._focused_workspace()
        if focused.get("name") == workspace_name:
            return ActiveWorkspace(
                id=focused["id"],
                name=workspace_name,
                restore_name=None,
                restore_reference=None,
                renamed_from_focused_workspace=False,
            )

        created_workspace = self._create_named_workspace_from_anchor(
            workspace_name,
            focused_before,
            restore_name,
            restore_reference,
            spawned_processes,
        )
        return created_workspace

    def _create_named_workspace_from_anchor(
        self,
        workspace_name: str,
        focused_before: dict[str, JsonValue],
        restore_name: str | None,
        restore_reference: dict[str, JsonValue] | None,
        spawned_processes: list[subprocess.Popen[str]],
    ) -> ActiveWorkspace:
        source_workspace_id = focused_before.get("id")
        if not isinstance(source_workspace_id, int):
            raise RunnerError("focused workspace is missing an integer id")

        windows_before = self._request_variant("Windows", "Windows")
        before_ids = {
            window.get("id") for window in windows_before if isinstance(window.get("id"), int)
        }

        self.event_monitor.reset_activity()
        spawned_processes.append(self._spawn_window())
        self._settle(f"workspace {workspace_name} anchor spawn")

        anchor_window_id = self._wait_for_new_window_id(
            before_ids,
            context=f"workspace {workspace_name} anchor window",
        )
        if anchor_window_id is None:
            raise RunnerError("could not determine anchor window id for temporary workspace")

        self._send_action("FocusWindow", {"id": anchor_window_id})
        self._wait_for_quiet(f"workspace {workspace_name} anchor focus")

        focused_after_move = self._move_anchor_to_adjacent_workspace(
            workspace_name,
            source_workspace_id,
        )
        target_workspace_id = focused_after_move.get("id")
        if not isinstance(target_workspace_id, int):
            raise RunnerError("temporary workspace is missing an integer id")

        self._send_action(
            "SetWorkspaceName",
            {
                "name": workspace_name,
                "workspace": {"Id": target_workspace_id},
            },
        )
        self._wait_for_quiet(f"workspace {workspace_name} rename")
        focused = self._focused_workspace(expected_name=workspace_name)
        return ActiveWorkspace(
            id=focused["id"],
            name=workspace_name,
            restore_name=restore_name,
            restore_reference=restore_reference,
            renamed_from_focused_workspace=False,
        )

    def _detect_new_window_id(self, before_ids: set[int]) -> int | None:
        windows_after = self._request_variant("Windows", "Windows")
        new_ids = [
            window.get("id")
            for window in windows_after
            if isinstance(window.get("id"), int) and window.get("id") not in before_ids
        ]
        if not new_ids:
            return None
        return max(new_ids)

    def _wait_for_new_window_id(self, before_ids: set[int], context: str) -> int | None:
        deadline = time.monotonic() + self.args.settle_timeout
        while time.monotonic() < deadline:
            self.event_monitor.raise_if_failed()
            window_id = self._detect_new_window_id(before_ids)
            if window_id is not None:
                return window_id
            time.sleep(self.args.settle_interval)
        return self._detect_new_window_id(before_ids)

    def _move_anchor_to_adjacent_workspace(
        self,
        workspace_name: str,
        source_workspace_id: int,
    ) -> dict[str, JsonValue]:
        for action_name in ("MoveWindowToWorkspaceDown", "MoveWindowToWorkspaceUp"):
            self.event_monitor.reset_activity()
            self._send_action(action_name, {"focus": True})
            self._wait_for_quiet(f"workspace {workspace_name} {action_name}")
            focused = self._focused_workspace()
            if focused.get("id") != source_workspace_id:
                return focused

        raise RunnerError(
            f"could not move anchor window away from source workspace {source_workspace_id}"
        )

    def _cleanup_workspace(self, workspace: ActiveWorkspace) -> None:
        self._focus_workspace_reference({"Id": workspace.id})
        self._wait_for_quiet(f"cleanup focus {workspace.name}")
        workspace_id = workspace.id

        for window in self._workspace_windows(workspace_id):
            self.event_monitor.reset_activity()
            self._send_action("CloseWindow", {"id": window["id"]})
            try:
                self._settle(f"cleanup close window {window['id']}")
            except RunnerError:
                break

        if workspace.renamed_from_focused_workspace:
            self.event_monitor.reset_activity()
            if workspace.restore_name:
                self._send_action(
                    "SetWorkspaceName",
                    {
                        "name": workspace.restore_name,
                        "workspace": {"Id": workspace.id},
                    },
                )
            else:
                self._send_action("UnsetWorkspaceName", {"reference": {"Id": workspace.id}})
            self._wait_for_quiet(f"cleanup restore workspace {workspace.id}")

    def _workspace_windows(self, workspace_id: int) -> list[dict[str, JsonValue]]:
        windows = self._request_variant("Windows", "Windows")
        filtered = [window for window in windows if window.get("workspace_id") == workspace_id]
        filtered.sort(key=lambda window: window.get("id", 0), reverse=True)
        return filtered

    def _terminate_processes(self, processes: list[subprocess.Popen[str]]) -> None:
        for proc in processes:
            if proc.poll() is not None:
                continue
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                continue
        deadline = time.monotonic() + 2.0
        for proc in processes:
            if proc.poll() is not None:
                continue
            remaining = max(0.0, deadline - time.monotonic())
            try:
                proc.wait(timeout=remaining)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                try:
                    proc.wait(timeout=1.0)
                except subprocess.TimeoutExpired:
                    pass

    def _build_summary(self, runs: list[dict[str, JsonValue]]) -> dict[str, JsonValue]:
        measured_runs = [run for run in runs if not run["warmup"]]
        aggregate_runs = measured_runs or runs
        aggregate: list[dict[str, JsonValue]] = []
        if aggregate_runs:
            step_count = len(aggregate_runs[0]["steps"])
            for index in range(step_count):
                durations = [run["steps"][index]["duration_ms"] for run in aggregate_runs]
                sample = aggregate_runs[0]["steps"][index]
                aggregate.append(
                    {
                        "sequence": sample["sequence"],
                        "label": sample["label"],
                        "kind": sample["kind"],
                        "action_name": sample["action_name"],
                        "sample_count": len(durations),
                        "p50_ms": percentile(durations, 0.50),
                        "p95_ms": percentile(durations, 0.95),
                        "max_ms": max(durations),
                        "durations_ms": durations,
                    }
                )

        return {
            "scenario": {
                "name": self.scenario.name,
                "path": os.fspath(self.args.scenario.resolve()),
                "initial_windows": self.scenario.initial_windows,
                "step_count": len(self.scenario.steps),
            },
            "socket_path": os.fspath(self.socket_path),
            "window_cmd": self.args.window_cmd,
            "repeat_count": self.args.repeat,
            "measured_run_count": len(aggregate_runs),
            "warmup_run_index": 0,
            "aggregates_exclude_warmup": bool(measured_runs),
            "settle_timeout_s": self.args.settle_timeout,
            "settle_interval_s": self.args.settle_interval,
            "idle_grace_s": self.args.idle_grace,
            "workspace_prefix": self.args.workspace_prefix,
            "cleanup": self.args.cleanup,
            "runs": runs,
            "step_summaries": aggregate,
        }

    def _write_summary(self, summary: dict[str, JsonValue]) -> None:
        summary_json = self.output_dir / "summary.json"
        summary_csv = self.output_dir / "summary.csv"
        summary_json.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

        with summary_csv.open("w", encoding="utf-8", newline="") as handle:
            writer = csv.DictWriter(
                handle,
                fieldnames=[
                    "run_index",
                    "warmup",
                    "workspace_name",
                    "sequence",
                    "label",
                    "kind",
                    "action_name",
                    "duration_ms",
                    "total_windows",
                    "workspace_windows",
                    "focused_window_id",
                ],
            )
            writer.writeheader()
            for run in summary["runs"]:
                for step in run["steps"]:
                    writer.writerow(
                        {
                            "run_index": run["run_index"],
                            "warmup": run["warmup"],
                            "workspace_name": run["workspace_name"],
                            "sequence": step["sequence"],
                            "label": step["label"],
                            "kind": step["kind"],
                            "action_name": step["action_name"],
                            "duration_ms": f"{step['duration_ms']:.3f}",
                            "total_windows": step["total_windows"],
                            "workspace_windows": step["workspace_windows"],
                            "focused_window_id": step["focused_window_id"],
                        }
                    )

    def _print_summary(self, summary: dict[str, JsonValue]) -> None:
        print(f"Scenario: {summary['scenario']['name']}")
        print(f"Socket:   {summary['socket_path']}")
        print(
            f"Runs:     {summary['repeat_count']} total, "
            f"{summary['measured_run_count']} measured "
            f"(warmup run index {summary['warmup_run_index']})"
        )
        print(f"Output:   {self.output_dir}")
        print()
        print("Step timings:")
        for step in summary["step_summaries"]:
            action = step["action_name"] or "-"
            print(
                f"  [{step['sequence']:02d}] {step['label']}: "
                f"p50={step['p50_ms']:.3f} ms "
                f"p95={step['p95_ms']:.3f} ms "
                f"max={step['max_ms']:.3f} ms "
                f"kind={step['kind']} action={action}"
            )


def load_scenario(path: Path) -> ScenarioSpec:
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as err:
        raise RunnerError(f"scenario file not found: {path}") from err
    except json.JSONDecodeError as err:
        raise RunnerError(f"invalid scenario JSON: {err}") from err

    if not isinstance(raw, dict):
        raise RunnerError("scenario must be a JSON object")

    name = raw.get("name") or path.stem
    if not isinstance(name, str) or not name.strip():
        raise RunnerError("scenario name must be a non-empty string")

    initial_windows = raw.get("initial_windows", 0)
    if not isinstance(initial_windows, int) or initial_windows < 0:
        raise RunnerError("initial_windows must be a non-negative integer")

    raw_steps = raw.get("steps", [])
    if not isinstance(raw_steps, list):
        raise RunnerError("steps must be a list")

    steps: list[StepSpec] = []
    for sequence, raw_step in enumerate(raw_steps):
        if not isinstance(raw_step, dict):
            raise RunnerError(f"step {sequence} must be an object")

        kind = raw_step.get("kind")
        if kind not in {"spawn_window", "action"}:
            raise RunnerError(f"step {sequence} has invalid kind: {kind!r}")

        label = raw_step.get("label")
        if label is None:
            label = default_step_label(sequence, kind, raw_step.get("name"))
        if not isinstance(label, str) or not label:
            raise RunnerError(f"step {sequence} has an invalid label")

        if kind == "spawn_window":
            steps.append(StepSpec(sequence=sequence, kind=kind, label=label))
            continue

        action_name = raw_step.get("name")
        if not isinstance(action_name, str) or not action_name:
            raise RunnerError(f"step {sequence} action needs a non-empty name")

        action_args = raw_step.get("args")
        if action_args is not None and not isinstance(action_args, dict):
            raise RunnerError(f"step {sequence} action args must be an object when provided")

        steps.append(
            StepSpec(
                sequence=sequence,
                kind=kind,
                label=label,
                action_name=action_name,
                action_args=action_args,
            )
        )

    return ScenarioSpec(name=name, initial_windows=initial_windows, steps=steps)


def default_step_label(sequence: int, kind: str, action_name: str | None) -> str:
    if kind == "spawn_window":
        return f"spawn_window_{sequence + 1}"
    if action_name:
        return f"{action_name}_{sequence + 1}"
    return f"step_{sequence + 1}"


def build_workspace_name(prefix: str, scenario_name: str, run_index: int) -> str:
    scenario_slug = slugify(scenario_name)
    timestamp = int(time.time() * 1000)
    return f"{prefix}-{scenario_slug}-{timestamp}-{run_index + 1}"


def slugify(value: str) -> str:
    out = []
    last_dash = False
    for ch in value.lower():
        if ch.isalnum():
            out.append(ch)
            last_dash = False
        elif not last_dash:
            out.append("-")
            last_dash = True
    slug = "".join(out).strip("-")
    return slug or "scenario"


def workspace_reference(workspace: dict[str, JsonValue] | None) -> dict[str, JsonValue] | None:
    if workspace is None:
        return None
    ws_id = workspace.get("id")
    if isinstance(ws_id, int):
        return {"Id": ws_id}
    name = workspace.get("name")
    if isinstance(name, str) and name:
        return {"Name": name}
    idx = workspace.get("idx")
    if isinstance(idx, int):
        return {"Index": idx}
    return None


def normalize_snapshot(
    windows: list[dict[str, JsonValue]],
    layout_tree: dict[str, JsonValue],
    workspace_id: int | None,
) -> dict[str, JsonValue]:
    normalized_windows = [normalize_window(window) for window in windows]
    normalized_windows.sort(key=lambda window: window["id"])

    focused_window_id = next(
        (window["id"] for window in normalized_windows if window["is_focused"]),
        None,
    )
    workspace_window_count = 0
    if workspace_id is not None:
        workspace_window_count = sum(
            1 for window in normalized_windows if window["workspace_id"] == workspace_id
        )

    return {
        "window_count": len(normalized_windows),
        "workspace_window_count": workspace_window_count,
        "focused_window_id": focused_window_id,
        "windows": normalized_windows,
        "layout_tree": layout_tree,
    }


def normalize_window(window: dict[str, JsonValue]) -> dict[str, JsonValue]:
    layout = window.get("layout")
    if not isinstance(layout, dict):
        layout = {}
    return {
        "id": window.get("id"),
        "title": window.get("title"),
        "app_id": window.get("app_id"),
        "workspace_id": window.get("workspace_id"),
        "is_focused": bool(window.get("is_focused")),
        "is_floating": bool(window.get("is_floating")),
        "is_urgent": bool(window.get("is_urgent")),
        "layout": layout,
    }


def strip_nulls(value: JsonValue) -> JsonValue:
    if isinstance(value, dict):
        return {key: strip_nulls(inner) for key, inner in value.items() if inner is not None}
    if isinstance(value, list):
        return [strip_nulls(inner) for inner in value]
    return value


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


def step_result_to_json(step: StepResult) -> dict[str, JsonValue]:
    return {
        "sequence": step.sequence,
        "label": step.label,
        "kind": step.kind,
        "action_name": step.action_name,
        "duration_ms": step.duration_ms,
        "total_windows": step.total_windows,
        "workspace_windows": step.workspace_windows,
        "focused_window_id": step.focused_window_id,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run a repeatable profiling scenario on tiri")
    parser.add_argument("--scenario", type=Path, required=True, help="Scenario JSON file")
    parser.add_argument(
        "--window-cmd",
        help="Command used for spawn_window steps and initial windows",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        required=True,
        help="Directory where summary.json and summary.csv will be written",
    )
    parser.add_argument(
        "--repeat",
        type=int,
        default=5,
        help="Number of scenario runs to record (default: 5)",
    )
    parser.add_argument(
        "--settle-timeout",
        type=float,
        default=2.0,
        help="Maximum seconds to wait for a step to settle (default: 2.0)",
    )
    parser.add_argument(
        "--settle-interval",
        type=float,
        default=0.02,
        help="Polling interval in seconds while waiting for settle (default: 0.02)",
    )
    parser.add_argument(
        "--idle-grace",
        type=float,
        default=0.10,
        help="Required quiet period with no IPC events before a step is stable (default: 0.10)",
    )
    parser.add_argument(
        "--workspace-prefix",
        default="PERF",
        help="Prefix used for temporary workspace names (default: PERF)",
    )
    parser.add_argument(
        "--socket",
        help="Path to the tiri IPC socket; defaults to $TIRI_SOCKET",
    )
    parser.add_argument(
        "--cleanup",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Close leftover windows and restore the original workspace when done",
    )
    parser.add_argument(
        "--validate-only",
        action="store_true",
        help="Validate the scenario JSON and exit without touching a live session",
    )
    args = parser.parse_args()

    if args.repeat < 1:
        parser.error("--repeat must be >= 1")
    if args.settle_timeout <= 0:
        parser.error("--settle-timeout must be > 0")
    if args.settle_interval <= 0:
        parser.error("--settle-interval must be > 0")
    if args.idle_grace < 0:
        parser.error("--idle-grace must be >= 0")
    if not args.validate_only and not args.window_cmd:
        parser.error("--window-cmd is required unless --validate-only is used")

    return args


def main() -> int:
    args = parse_args()
    try:
        scenario = load_scenario(args.scenario)
        if args.validate_only:
            print(
                f"Scenario OK: {scenario.name} "
                f"(initial_windows={scenario.initial_windows}, steps={len(scenario.steps)})"
            )
            return 0

        runner = Runner(args, scenario)
        return runner.run()
    except RunnerError as err:
        print(f"ERROR: {err}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

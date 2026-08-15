#!/usr/bin/env python3
"""Capture latency and process efficiency from a Tracy-enabled tiri DRM session."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import IO

sys.dont_write_bytecode = True


class ProfileError(RuntimeError):
    pass


ROOT = Path(__file__).resolve().parents[1]


@dataclass(frozen=True)
class ProcessSnapshot:
    sampled_at_monotonic_ns: int
    sampled_at_boottime_ns: int
    start_time_ticks: int
    user_time_ticks: int
    system_time_ticks: int
    minor_page_faults: int
    major_page_faults: int
    voluntary_context_switches: int
    involuntary_context_switches: int


def parse_proc_stat(contents: str) -> dict[str, int]:
    # comm is parenthesized and may itself contain spaces or closing parentheses.
    closing_parenthesis = contents.rfind(")")
    if closing_parenthesis < 0:
        raise ProfileError("invalid /proc stat: missing process name")
    fields = contents[closing_parenthesis + 1 :].split()
    if len(fields) < 20:
        raise ProfileError("invalid /proc stat: missing process counters")
    try:
        return {
            "minor_page_faults": int(fields[7]),
            "major_page_faults": int(fields[9]),
            "user_time_ticks": int(fields[11]),
            "system_time_ticks": int(fields[12]),
            "start_time_ticks": int(fields[19]),
        }
    except ValueError as error:
        raise ProfileError(f"invalid /proc stat counter: {error}") from error


def parse_proc_status(contents: str) -> dict[str, int]:
    wanted = {
        "voluntary_ctxt_switches": "voluntary_context_switches",
        "nonvoluntary_ctxt_switches": "involuntary_context_switches",
    }
    counters: dict[str, int] = {}
    for line in contents.splitlines():
        name, separator, value = line.partition(":")
        output_name = wanted.get(name)
        if separator and output_name is not None:
            try:
                counters[output_name] = int(value.strip())
            except ValueError as error:
                raise ProfileError(f"invalid /proc status counter {name}: {value}") from error
    missing = set(wanted.values()) - counters.keys()
    if missing:
        raise ProfileError(f"invalid /proc status: missing {sorted(missing)}")
    return counters


def read_process_snapshot(pid: int, proc_root: Path = Path("/proc")) -> ProcessSnapshot:
    process_dir = proc_root / str(pid)
    stat = parse_proc_stat((process_dir / "stat").read_text(encoding="utf-8"))
    status = parse_proc_status((process_dir / "status").read_text(encoding="utf-8"))
    boottime_clock = getattr(time, "CLOCK_BOOTTIME", time.CLOCK_MONOTONIC)
    return ProcessSnapshot(
        sampled_at_monotonic_ns=time.monotonic_ns(),
        sampled_at_boottime_ns=time.clock_gettime_ns(boottime_clock),
        **stat,
        **status,
    )


def summarize_process_efficiency(
    start: ProcessSnapshot,
    end: ProcessSnapshot,
    clock_ticks_per_second: int,
) -> dict[str, object]:
    if clock_ticks_per_second <= 0:
        raise ProfileError("invalid system clock tick rate")
    if start.start_time_ticks != end.start_time_ticks:
        raise ProfileError("compositor process changed during the workload")
    wall_time_s = (end.sampled_at_monotonic_ns - start.sampled_at_monotonic_ns) / 1e9
    if wall_time_s <= 0:
        raise ProfileError("process measurement window is empty")

    counter_names = (
        "user_time_ticks",
        "system_time_ticks",
        "minor_page_faults",
        "major_page_faults",
        "voluntary_context_switches",
        "involuntary_context_switches",
    )
    deltas = {name: getattr(end, name) - getattr(start, name) for name in counter_names}
    negative = [name for name, value in deltas.items() if value < 0]
    if negative:
        raise ProfileError(f"process counters went backwards: {negative}")

    user_cpu_s = deltas["user_time_ticks"] / clock_ticks_per_second
    system_cpu_s = deltas["system_time_ticks"] / clock_ticks_per_second
    total_cpu_s = user_cpu_s + system_cpu_s
    process_start_boottime_ns = (
        start.start_time_ticks * 1_000_000_000 // clock_ticks_per_second
    )
    start_process_elapsed_ns = start.sampled_at_boottime_ns - process_start_boottime_ns
    end_process_elapsed_ns = end.sampled_at_boottime_ns - process_start_boottime_ns
    if start_process_elapsed_ns < 0 or end_process_elapsed_ns <= start_process_elapsed_ns:
        raise ProfileError("invalid compositor-relative measurement window")

    voluntary = deltas["voluntary_context_switches"]
    involuntary = deltas["involuntary_context_switches"]
    return {
        "format": 1,
        "source": "/proc/<pid>/stat and status",
        "context_switch_scope": "thread-group leader",
        "clock_ticks_per_second": clock_ticks_per_second,
        "measurement_start_process_elapsed_ns": start_process_elapsed_ns,
        "measurement_end_process_elapsed_ns": end_process_elapsed_ns,
        "wall_time_s": wall_time_s,
        "user_cpu_s": user_cpu_s,
        "system_cpu_s": system_cpu_s,
        "total_cpu_s": total_cpu_s,
        "average_cpu_cores": total_cpu_s / wall_time_s,
        "average_cpu_percent_of_one_core": total_cpu_s / wall_time_s * 100.0,
        "minor_page_faults": deltas["minor_page_faults"],
        "major_page_faults": deltas["major_page_faults"],
        "voluntary_context_switches": voluntary,
        "involuntary_context_switches": involuntary,
        "context_switches": voluntary + involuntary,
    }


def export_trace(exporter: str, trace: Path, output_dir: Path) -> None:
    exports = (
        ("plots.csv", ["-u", "-p"]),
        ("messages.csv", ["-m"]),
    )
    for name, flags in exports:
        started_at = time.monotonic()
        print(f"Exporting Tracy data to {name}...", flush=True)
        with (output_dir / name).open("wb") as output:
            subprocess.run([exporter, *flags, os.fspath(trace)], check=True, stdout=output)
        elapsed = time.monotonic() - started_at
        print(f"Exported {name} in {elapsed:.1f} seconds.", flush=True)


def stop_capture(process: subprocess.Popen[bytes], timeout: float = 15.0) -> None:
    if process.poll() is not None:
        return
    process.send_signal(signal.SIGINT)
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5.0)


def validate_session_environment() -> tuple[Path, str]:
    socket_value = os.environ.get("TIRI_SOCKET")
    display = os.environ.get("WAYLAND_DISPLAY")
    if not socket_value:
        raise ProfileError("TIRI_SOCKET is not set; run this from inside the tiri session")
    if not display:
        raise ProfileError("WAYLAND_DISPLAY is not set; run this from inside the tiri session")
    socket_path = Path(socket_value)
    if not socket_path.exists():
        raise ProfileError(f"tiri IPC socket does not exist: {socket_path}")
    return socket_path, display


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def identify_compositor(socket_path: Path, expected_exe: Path | None) -> dict[str, object]:
    credentials_size = struct.calcsize("3i")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as peer:
        peer.settimeout(2.0)
        peer.connect(os.fspath(socket_path))
        credentials = peer.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, credentials_size)
    pid, uid, gid = struct.unpack("3i", credentials)

    proc_exe = Path(f"/proc/{pid}/exe")
    executable = os.readlink(proc_exe)
    executable_sha256 = sha256_file(proc_exe)
    identity: dict[str, object] = {
        "pid": pid,
        "uid": uid,
        "gid": gid,
        "executable": executable,
        "sha256": executable_sha256,
    }

    if expected_exe is not None:
        expected_exe = expected_exe.resolve(strict=True)
        expected_sha256 = sha256_file(expected_exe)
        identity["expected_executable"] = os.fspath(expected_exe)
        identity["expected_sha256"] = expected_sha256
        if executable_sha256 != expected_sha256:
            raise ProfileError(
                "the compositor serving TIRI_SOCKET is not the expected binary: "
                f"running {executable} ({executable_sha256}), expected {expected_exe} "
                f"({expected_sha256})"
            )

    return identity


def run(args: argparse.Namespace) -> int:
    socket_path, display = validate_session_environment()
    compositor = identify_compositor(socket_path, args.expected_exe)
    output_dir = args.output_dir.resolve()
    if output_dir.exists() and any(output_dir.iterdir()):
        raise ProfileError(f"output directory is not empty: {output_dir}")
    output_dir.mkdir(parents=True, exist_ok=True)

    capture_bin = shutil.which(args.tracy_capture)
    exporter_bin = shutil.which(args.tracy_export)
    if capture_bin is None:
        raise ProfileError(f"Tracy capture binary not found: {args.tracy_capture}")
    if exporter_bin is None:
        raise ProfileError(f"Tracy CSV exporter not found: {args.tracy_export}")

    trace = output_dir / "capture.tracy"
    capture_log_path = output_dir / "capture.log"
    capture_log: IO[bytes] | None = None
    capture_process: subprocess.Popen[bytes] | None = None
    efficiency_start: ProcessSnapshot | None = None
    efficiency_end: ProcessSnapshot | None = None
    started_at = time.time()

    try:
        capture_log = capture_log_path.open("wb")
        capture_process = subprocess.Popen(
            [
                capture_bin,
                "-a",
                args.tracy_address,
                "-p",
                str(args.tracy_port),
                "-o",
                os.fspath(trace),
            ],
            stdout=capture_log,
            stderr=subprocess.STDOUT,
        )
        time.sleep(args.capture_warmup)
        if capture_process.poll() is not None:
            raise ProfileError(
                f"tracy-capture exited before profiling (status {capture_process.returncode})"
            )
        efficiency_start = read_process_snapshot(int(compositor["pid"]))

        if args.scenario is not None:
            scenario_dir = output_dir / "scenario"
            subprocess.run(
                [
                    sys.executable,
                    os.fspath(ROOT / "scripts/profile_tiri_scenario.py"),
                    "--scenario",
                    os.fspath(args.scenario.resolve()),
                    "--window-cmd",
                    args.window_cmd,
                    "--output-dir",
                    os.fspath(scenario_dir),
                    "--repeat",
                    str(args.repeat),
                    "--settle-timeout",
                    str(args.settle_timeout),
                    "--ipc-timeout",
                    str(args.ipc_timeout),
                    "--workspace-prefix",
                    args.workspace_prefix,
                ],
                check=True,
            )
        else:
            print(f"Capturing real input for {args.duration:.1f} seconds.", flush=True)
            if args.manual_task:
                print(f"Task: {args.manual_task}", flush=True)
            deadline = time.monotonic() + args.duration
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                if capture_process.poll() is not None:
                    raise ProfileError(
                        "tracy-capture exited during profiling "
                        f"(status {capture_process.returncode})"
                    )
                time.sleep(min(remaining, 0.25))
        efficiency_end = read_process_snapshot(int(compositor["pid"]))
    finally:
        if capture_process is not None:
            stop_capture(capture_process)
        if capture_log is not None:
            capture_log.close()

    if not trace.is_file() or trace.stat().st_size == 0:
        log = capture_log_path.read_text(encoding="utf-8", errors="replace")
        raise ProfileError(f"Tracy did not produce a trace:\n{log}")
    if efficiency_start is None or efficiency_end is None:
        raise ProfileError("process efficiency measurement did not complete")

    clock_ticks_per_second = int(os.sysconf("SC_CLK_TCK"))
    process_efficiency = summarize_process_efficiency(
        efficiency_start,
        efficiency_end,
        clock_ticks_per_second,
    )

    export_trace(exporter_bin, trace, output_dir)
    metadata = {
        "format": 2,
        "started_at_unix": started_at,
        "finished_at_unix": time.time(),
        "socket": os.fspath(socket_path),
        "wayland_display": display,
        "compositor": compositor,
        "mode": "scenario" if args.scenario is not None else "manual",
        "scenario": os.fspath(args.scenario.resolve()) if args.scenario is not None else None,
        "window_cmd": args.window_cmd if args.scenario is not None else None,
        "repeat": args.repeat if args.scenario is not None else None,
        "duration_s": args.duration if args.scenario is None else None,
        "manual_task": args.manual_task if args.scenario is None else None,
        "trace_bytes": trace.stat().st_size,
        "process_efficiency": process_efficiency,
    }
    (output_dir / "metadata.json").write_text(
        json.dumps(metadata, indent=2) + "\n", encoding="utf-8"
    )

    report_path = output_dir / "perceptual.json"
    subprocess.run(
        [
            sys.executable,
            os.fspath(ROOT / "scripts/analyze_tiri_perceptual.py"),
            "analyze",
            os.fspath(output_dir),
            "--output",
            os.fspath(report_path),
        ],
        check=True,
    )
    report = json.loads(report_path.read_text(encoding="utf-8"))
    if report.get("backends") != ["drm"]:
        raise ProfileError(
            f"capture did not contain only physical DRM presentations: {report.get('backends')}"
        )
    if report.get("process_efficiency", {}).get("drm_presentations", 0) < 1:
        raise ProfileError("capture did not contain DRM vblank messages in the workload window")

    print(report_path)
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--scenario", type=Path)
    mode.add_argument("--duration", type=float)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--manual-task")
    parser.add_argument("--window-cmd", default="foot --app-id perceptual-test")
    parser.add_argument("--repeat", type=int, default=6)
    parser.add_argument("--settle-timeout", type=float, default=5.0)
    parser.add_argument("--ipc-timeout", type=float, default=5.0)
    parser.add_argument("--workspace-prefix", default="PERCEPTUAL")
    parser.add_argument("--capture-warmup", type=float, default=2.0)
    parser.add_argument("--tracy-address", default="127.0.0.1")
    parser.add_argument("--tracy-port", type=int, default=8086)
    parser.add_argument("--tracy-capture", default="tracy-capture")
    parser.add_argument("--tracy-export", default="tracy-csvexport")
    parser.add_argument(
        "--expected-exe",
        type=Path,
        help="reject the capture unless TIRI_SOCKET belongs to this exact executable hash",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.duration is not None and args.duration <= 0:
            raise ProfileError("--duration must be positive")
        if args.repeat < 1:
            raise ProfileError("--repeat must be positive")
        if args.ipc_timeout <= 0:
            raise ProfileError("--ipc-timeout must be positive")
        return run(args)
    except (
        OSError,
        ValueError,
        json.JSONDecodeError,
        subprocess.CalledProcessError,
        ProfileError,
    ) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Run a reproducible headless tiri scenario and capture its Tracy zones.

Unlike the older helpers, this runner owns the compositor, Tracy capture and scenario processes.
That keeps them in one execution environment, fixes socket discovery, and makes the produced trace
refer to the exact binary and output topology recorded in ``metadata.json``.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import IO


class ProfileError(RuntimeError):
    pass


CONFIG = """\
input {
    keyboard {
        xkb {
        }
    }
}

layout {
    gaps 8
    focus-ring {
        off
    }
    border {
        on
        width 6
    }
}

prefer-no-csd

hotkey-overlay {
    skip-at-startup
}

animations {
    off
}
"""


def wait_for_socket(runtime_dir: Path, pattern: str, process: subprocess.Popen[bytes]) -> Path:
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise ProfileError(f"tiri exited before creating {pattern} (status {process.returncode})")
        matches = sorted(runtime_dir.glob(pattern))
        if matches:
            return matches[0]
        time.sleep(0.02)
    raise ProfileError(f"timed out waiting for {pattern} in {runtime_dir}")


def stop_process(process: subprocess.Popen[bytes] | None, timeout: float = 10.0) -> None:
    if process is None or process.poll() is not None:
        return
    process.send_signal(signal.SIGINT)
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5.0)


def export_trace(exporter: str, trace: Path, out_dir: Path) -> None:
    exports = [
        ("cpu-zones-total.csv", ["-u"]),
        ("cpu-zones-self.csv", ["-u", "-e"]),
        ("cpu-zones-summary.csv", []),
        ("plots.csv", ["-u", "-p"]),
        ("messages.csv", ["-m"]),
    ]
    for name, flags in exports:
        with (out_dir / name).open("wb") as output:
            subprocess.run([exporter, *flags, os.fspath(trace)], check=True, stdout=output)


def run(args: argparse.Namespace) -> int:
    root = Path(__file__).resolve().parents[1]
    output_dir = args.output_dir.resolve()
    runtime_dir = output_dir / "runtime"
    scenario_dir = output_dir / "scenario"
    output_dir.mkdir(parents=True, exist_ok=True)
    runtime_dir.mkdir(mode=0o700, exist_ok=True)
    runtime_dir.chmod(0o700)
    scenario_dir.mkdir(exist_ok=True)

    config = output_dir / "config.kdl"
    config.write_text(CONFIG, encoding="utf-8")
    trace = output_dir / "capture.tracy"
    tiri_log_path = output_dir / "tiri.log"
    capture_log_path = output_dir / "capture.log"

    tiri = args.tiri.resolve()
    scenario_source = args.scenario.resolve()
    if not tiri.is_file():
        raise ProfileError(f"tiri binary does not exist: {tiri}")
    if not scenario_source.is_file():
        raise ProfileError(f"scenario does not exist: {scenario_source}")

    scenario = scenario_source
    if args.initial_windows is not None:
        scenario_data = json.loads(scenario_source.read_text(encoding="utf-8"))
        scenario_data["initial_windows"] = args.initial_windows
        scenario = output_dir / "scenario.effective.json"
        scenario.write_text(json.dumps(scenario_data, indent=2) + "\n", encoding="utf-8")

    capture_bin = shutil.which(args.tracy_capture)
    exporter_bin = shutil.which(args.tracy_export)
    if capture_bin is None:
        raise ProfileError(f"Tracy capture binary not found: {args.tracy_capture}")
    if exporter_bin is None:
        raise ProfileError(f"Tracy CSV exporter not found: {args.tracy_export}")

    environment = os.environ.copy()
    environment.update(
        {
            "XDG_RUNTIME_DIR": os.fspath(runtime_dir),
            "RUST_LOG": args.rust_log,
        }
    )
    environment.pop("WAYLAND_DISPLAY", None)
    environment.pop("TIRI_SOCKET", None)

    tiri_process: subprocess.Popen[bytes] | None = None
    capture_process: subprocess.Popen[bytes] | None = None
    tiri_log: IO[bytes] | None = None
    capture_log: IO[bytes] | None = None
    started_at = time.time()

    try:
        tiri_log = tiri_log_path.open("wb")
        tiri_process = subprocess.Popen(
            [
                os.fspath(tiri),
                "--headless",
                "--headless-outputs",
                str(args.outputs),
                "--headless-output-width",
                str(args.output_width),
                "--headless-output-height",
                str(args.output_height),
                "--config",
                os.fspath(config),
            ],
            env=environment,
            stdout=tiri_log,
            stderr=subprocess.STDOUT,
        )

        wayland_socket = wait_for_socket(runtime_dir, "wayland-*", tiri_process)
        ipc_socket = wait_for_socket(runtime_dir, "niri.*.sock", tiri_process)
        environment["WAYLAND_DISPLAY"] = wayland_socket.name
        environment["TIRI_SOCKET"] = os.fspath(ipc_socket)

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
                f"tracy-capture exited before the scenario (status {capture_process.returncode})"
            )

        subprocess.run(
            [
                sys.executable,
                os.fspath(root / "scripts/profile_tiri_scenario.py"),
                "--scenario",
                os.fspath(scenario),
                "--window-cmd",
                args.window_cmd,
                "--output-dir",
                os.fspath(scenario_dir),
                "--repeat",
                str(args.repeat),
                "--settle-timeout",
                str(args.settle_timeout),
                "--settle-interval",
                str(args.settle_interval),
                "--idle-grace",
                str(args.idle_grace),
                "--ipc-timeout",
                str(args.ipc_timeout),
                "--workspace-prefix",
                args.workspace_prefix,
            ],
            check=True,
            env=environment,
        )
    finally:
        stop_process(tiri_process)
        if tiri_log is not None:
            tiri_log.close()

        if capture_process is not None and capture_process.poll() is None:
            try:
                capture_process.wait(timeout=10.0)
            except subprocess.TimeoutExpired:
                stop_process(capture_process, timeout=2.0)
        if capture_log is not None:
            capture_log.close()

    if not trace.is_file() or trace.stat().st_size == 0:
        capture_log_text = capture_log_path.read_text(encoding="utf-8", errors="replace")
        raise ProfileError(f"Tracy did not produce a trace:\n{capture_log_text}")

    export_trace(exporter_bin, trace, output_dir)
    metadata = {
        "started_at_unix": started_at,
        "finished_at_unix": time.time(),
        "tiri": os.fspath(tiri),
        "tiri_version": subprocess.run(
            [os.fspath(tiri), "--version"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip(),
        "scenario": os.fspath(scenario),
        "scenario_source": os.fspath(scenario_source),
        "initial_windows_override": args.initial_windows,
        "repeat": args.repeat,
        "outputs": args.outputs,
        "output_size": [args.output_width, args.output_height],
        "window_cmd": args.window_cmd,
        "rust_log": args.rust_log,
        "ipc_timeout": args.ipc_timeout,
        "trace_bytes": trace.stat().st_size,
    }
    (output_dir / "metadata.json").write_text(
        json.dumps(metadata, indent=2) + "\n", encoding="utf-8"
    )
    print(output_dir)
    return 0


def parse_args() -> argparse.Namespace:
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tiri", type=Path, default=root / "target/release/tiri")
    parser.add_argument("--scenario", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--window-cmd", default="foot --app-id perf-test")
    parser.add_argument("--repeat", type=int, default=7)
    parser.add_argument("--initial-windows", type=int)
    parser.add_argument("--outputs", type=int, default=1)
    parser.add_argument("--output-width", type=int, default=1920)
    parser.add_argument("--output-height", type=int, default=1080)
    parser.add_argument("--workspace-prefix", default="PERF-HEADLESS")
    parser.add_argument("--settle-timeout", type=float, default=2.0)
    parser.add_argument("--settle-interval", type=float, default=0.02)
    parser.add_argument("--idle-grace", type=float, default=0.10)
    parser.add_argument("--ipc-timeout", type=float, default=5.0)
    parser.add_argument("--capture-warmup", type=float, default=1.0)
    parser.add_argument("--rust-log", default="warn")
    parser.add_argument("--tracy-address", default="127.0.0.1")
    parser.add_argument("--tracy-port", type=int, default=8086)
    parser.add_argument("--tracy-capture", default="tracy-capture")
    parser.add_argument("--tracy-export", default="tracy-csvexport")
    return parser.parse_args()


def main() -> int:
    try:
        return run(parse_args())
    except (OSError, subprocess.CalledProcessError, ProfileError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

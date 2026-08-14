#!/usr/bin/env python3
"""Capture perceptual latency from an already-running Tracy-enabled tiri DRM session."""

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
from pathlib import Path
from typing import IO

sys.dont_write_bytecode = True


class ProfileError(RuntimeError):
    pass


ROOT = Path(__file__).resolve().parents[1]


def export_trace(exporter: str, trace: Path, output_dir: Path) -> None:
    exports = (
        ("cpu-zones-summary.csv", []),
        ("plots.csv", ["-u", "-p"]),
        ("messages.csv", ["-m"]),
    )
    for name, flags in exports:
        with (output_dir / name).open("wb") as output:
            subprocess.run([exporter, *flags, os.fspath(trace)], check=True, stdout=output)


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
                        f"tracy-capture exited during profiling (status {capture_process.returncode})"
                    )
                time.sleep(min(remaining, 0.25))
    finally:
        if capture_process is not None:
            stop_capture(capture_process)
        if capture_log is not None:
            capture_log.close()

    if not trace.is_file() or trace.stat().st_size == 0:
        log = capture_log_path.read_text(encoding="utf-8", errors="replace")
        raise ProfileError(f"Tracy did not produce a trace:\n{log}")

    export_trace(exporter_bin, trace, output_dir)
    metadata = {
        "format": 1,
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
    except (OSError, ValueError, json.JSONDecodeError, subprocess.CalledProcessError, ProfileError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())

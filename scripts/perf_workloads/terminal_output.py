#!/usr/bin/env python3
"""Emit deterministic, paced terminal output until the terminal closes."""

from __future__ import annotations

import argparse
import sys
import time


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lines-per-second", type=float, default=30.0)
    args = parser.parse_args()
    if args.lines_per_second <= 0:
        parser.error("--lines-per-second must be positive")

    interval = 1.0 / args.lines_per_second
    deadline = time.monotonic()
    line = 0
    try:
        while True:
            checksum = (line * 2_654_435_761) & 0xFFFFFFFF
            phase = (line // 120) % 4
            sys.stdout.write(
                f"\x1b[3{phase + 2}m"
                f"build[{line:06d}] task={line % 23:02d} checksum={checksum:08x} "
                f"status={'done' if line % 7 == 0 else 'running'}"
                "\x1b[0m\n"
            )
            sys.stdout.flush()
            line += 1
            deadline += interval
            time.sleep(max(0.0, deadline - time.monotonic()))
    except (BrokenPipeError, KeyboardInterrupt):
        return 0


if __name__ == "__main__":
    raise SystemExit(main())

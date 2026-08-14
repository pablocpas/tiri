#!/usr/bin/env bash
set -eu

workload_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
exec foot \
    --app-id=perf-terminal \
    --title=tiri-terminal-activity \
    python3 "$workload_dir/terminal_output.py" --lines-per-second 30

#!/usr/bin/env bash
# Run a physical-backend P1 A-B-A campaign from one spare Linux VT.

set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT/scripts/profile_tiri_perceptual_ab.sh"
REAL_PROFILER="$ROOT/scripts/profile_tiri_real_tracy.py"
ANALYZER="$ROOT/scripts/analyze_tiri_perceptual.py"
BEFORE_BIN="$ROOT/target/perceptual-binaries/before/tiri"
AFTER_BIN="$ROOT/target/perceptual-binaries/after/tiri"

die() {
    echo "error: $*" >&2
    exit 1
}

inside_finish() {
    local result=$?
    trap - EXIT

    if [[ $result -ne 0 && ! -e "$INSIDE_STATUS" ]]; then
        printf 'failed:%s\n' "$result" >"$INSIDE_STATUS"
    fi

    # The socket belongs to this exact compositor, so this cannot quit KDE.
    sleep 1
    "$INSIDE_BINARY" msg action quit --skip-confirmation >/dev/null 2>&1 || true
    exit "$result"
}

run_real_scenario() {
    local name=$1
    local scenario=$2
    local window_cmd=$3
    local repeat=$4
    local output_dir="$INSIDE_OUTPUT/$name"

    echo
    echo "== Deterministic workload: $name =="
    python3 "$REAL_PROFILER" \
        --scenario "$scenario" \
        --window-cmd "$window_cmd" \
        --repeat "$repeat" \
        --expected-exe "$INSIDE_BINARY" \
        --output-dir "$output_dir"
    # Tracy may still be finishing asynchronous source-code transfers after the capture
    # process has written the trace.  Give the on-demand client time to return to its
    # listening state before opening the next connection.
    sleep 5
}

probe_tracy_reconnect() {
    echo
    echo "== Tracy on-demand reconnect preflight =="
    local attempt trace log capture_pid capture_status
    for attempt in 1 2; do
        trace="$INSIDE_OUTPUT/tracy-preflight-$attempt.tracy"
        log="$INSIDE_OUTPUT/tracy-preflight-$attempt.log"
        tracy-capture -a 127.0.0.1 -p 8086 -o "$trace" >"$log" 2>&1 &
        capture_pid=$!
        # A one-second connection can be interrupted while Tracy is still transferring
        # executable/source metadata.  Exercise the same steady state as a real workload.
        sleep 3
        if ! kill -0 "$capture_pid" 2>/dev/null; then
            set +e
            wait "$capture_pid"
            capture_status=$?
            set -e
            cat "$log" >&2
            die "Tracy reconnect probe $attempt exited early (status $capture_status); the binary must use profile-with-tracy-ondemand"
        fi
        kill -INT "$capture_pid"
        set +e
        wait "$capture_pid"
        capture_status=$?
        set -e
        if [[ $capture_status -ne 0 || ! -s $trace ]]; then
            cat "$log" >&2
            die "Tracy reconnect probe $attempt failed (status $capture_status)"
        fi
        rm -f "$trace"
        sleep 5
        "$INSIDE_BINARY" msg -j version >/dev/null ||
            die "Tiri stopped responding after Tracy reconnect probe $attempt"
    done
    echo "Two consecutive Tracy connections succeeded."
}

inside_main() {
    [[ $# -eq 5 ]] || die "invalid internal invocation"
    local label=$1
    INSIDE_BINARY=$2
    INSIDE_OUTPUT=$3
    local manual_duration=$4
    local scenario_repeat=$5
    INSIDE_STATUS="$INSIDE_OUTPUT/status"

    mkdir -p "$INSIDE_OUTPUT"
    trap inside_finish EXIT

    echo "P1 physical presentation campaign: $label"
    echo "Executable: $INSIDE_BINARY"
    echo "Output:     $INSIDE_OUTPUT"
    echo
    echo "First, the script will run three deterministic workflows."
    echo "Do not interact with their windows; this part must be identical in every session."
    echo "If Tiri showed the hotkey overlay, close it now with Escape."
    sleep 5

    probe_tracy_reconnect

    local open_close_repeat=$scenario_repeat
    if ((open_close_repeat < 5)); then
        open_close_repeat=5
    fi

    run_real_scenario \
        "open-close" \
        "$ROOT/scripts/perf_scenarios/open_close.json" \
        "foot --app-id perf-open-close" \
        "$open_close_repeat"
    run_real_scenario \
        "terminal-activity" \
        "$ROOT/scripts/perf_scenarios/terminal_activity.json" \
        "$ROOT/scripts/perf_workloads/run_terminal_activity.sh" \
        "$scenario_repeat"
    run_real_scenario \
        "browser-activity" \
        "$ROOT/scripts/perf_scenarios/browser_activity.json" \
        "$ROOT/scripts/perf_workloads/run_browser_activity.sh" \
        "$scenario_repeat"

    echo
    echo "== Normal-work physical-input capture =="
    echo "Duration: $manual_duration seconds"
    echo
    echo "Use Tiri normally, but try to include all of these during every A-B-A pass:"
    echo "  - type and scroll in a terminal"
    echo "  - browse, change tabs and scroll a page"
    echo "  - open, focus, fullscreen and close a few applications"
    echo "  - use only disposable work: Tiri will close automatically when time expires"
    echo
    echo "The exact key rhythm need not match: results are grouped by input source, and"
    echo "the two BEFORE passes bracket AFTER to expose human/thermal drift."
    read -r -p "Press Enter when your terminal and browser are ready... "

    python3 "$REAL_PROFILER" \
        --duration "$manual_duration" \
        --capture-warmup 3 \
        --manual-task "normal terminal, browser and window-management work ($label)" \
        --expected-exe "$INSIDE_BINARY" \
        --output-dir "$INSIDE_OUTPUT/manual-normal"

    python3 - "$INSIDE_OUTPUT/manual-normal/perceptual.json" <<'PY'
import json
import sys

report = json.load(open(sys.argv[1], encoding="utf-8"))
samples = report["physical_input"]["samples"]
print(f"Physical-input samples captured: {samples}")
if samples < 30:
    print("WARNING: fewer than 30 physical-input samples; interact more in the next pass.")
PY

    printf 'ok\n' >"$INSIDE_STATUS"

    echo
    echo "Capture $label complete."
    echo "Tiri will close automatically in five seconds."
    sleep 5
}

if [[ ${1:-} == "--inside" ]]; then
    shift
    inside_main "$@"
    exit 0
fi

usage() {
    cat <<EOF
Usage: $0 [OPTIONS]

Run from a spare text VT (for example Ctrl+Alt+F3), not from Konsole or Tiri.
The order is BEFORE-A -> AFTER -> BEFORE-B.

Options:
  --duration SECONDS       normal-work capture per compositor (default: 120)
  --scenario-repeat COUNT  deterministic scenario repetitions (default: 3)
  --output-dir PATH        campaign directory (default: timestamp under target/)
  -h, --help               show this help
EOF
}

MANUAL_DURATION=120
SCENARIO_REPEAT=3
OUTPUT_DIR=""
while [[ $# -gt 0 ]]; do
    case $1 in
        --duration)
            [[ $# -ge 2 ]] || die "--duration requires a value"
            MANUAL_DURATION=$2
            shift 2
            ;;
        --scenario-repeat)
            [[ $# -ge 2 ]] || die "--scenario-repeat requires a value"
            SCENARIO_REPEAT=$2
            shift 2
            ;;
        --output-dir)
            [[ $# -ge 2 ]] || die "--output-dir requires a value"
            OUTPUT_DIR=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1"
            ;;
    esac
done

[[ $MANUAL_DURATION =~ ^[1-9][0-9]*$ ]] || die "--duration must be a positive integer"
[[ $SCENARIO_REPEAT =~ ^[1-9][0-9]*$ ]] || die "--scenario-repeat must be a positive integer"

current_tty="$(tty 2>/dev/null || true)"
[[ $current_tty =~ ^/dev/tty[0-9]+$ ]] || die \
    "run this from a spare text VT, not from a terminal inside KDE/Tiri ($current_tty)"
[[ -z ${DISPLAY:-} && -z ${WAYLAND_DISPLAY:-} && -z ${WAYLAND_SOCKET:-} ]] || die \
    "graphical display variables are set; log in directly on the spare VT"

for command in python3 foot tracy-capture tracy-csvexport brave-browser-stable; do
    command -v "$command" >/dev/null || die "required command not found: $command"
done
for path in "$BEFORE_BIN" "$AFTER_BIN" "$REAL_PROFILER" "$ANALYZER"; do
    [[ -e $path ]] || die "required file not found: $path"
done

if [[ -z $OUTPUT_DIR ]]; then
    OUTPUT_DIR="$ROOT/target/perceptual-runs/aba-$(date +%Y%m%d-%H%M%S)"
elif [[ $OUTPUT_DIR != /* ]]; then
    OUTPUT_DIR="$PWD/$OUTPUT_DIR"
fi
if [[ -d $OUTPUT_DIR && -n $(find "$OUTPUT_DIR" -mindepth 1 -maxdepth 1 -print -quit) ]]; then
    die "output directory is not empty: $OUTPUT_DIR"
fi
mkdir -p "$OUTPUT_DIR"

run_variant() {
    local label=$1
    local binary=$2
    local variant_dir="$OUTPUT_DIR/$label"
    local status_file="$variant_dir/status"

    echo
    echo "============================================================"
    echo "Ready to start $label"
    echo "Binary: $binary"
    echo "When Tiri opens, press Escape if the hotkey overlay is visible."
    echo "The controller terminal will then run the deterministic suite."
    echo "============================================================"
    read -r -p "Press Enter to launch $label... "

    set +e
    env -u DISPLAY -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
        "$binary" -- \
        foot --app-id=tiri-perceptual-controller --title="P1 campaign: $label" \
        bash "$SCRIPT" --inside "$label" "$binary" "$variant_dir" \
        "$MANUAL_DURATION" "$SCENARIO_REPEAT"
    local compositor_result=$?
    set -e

    if [[ ! -f $status_file ]]; then
        die "$label ended without a session status (tiri exit status $compositor_result)"
    fi
    local status
    status="$(<"$status_file")"
    [[ $status == ok ]] || die "$label capture failed: $status"
    echo "$label captured successfully."
}

cat >"$OUTPUT_DIR/campaign.txt" <<EOF
order=before-a,after,before-b
manual_duration_seconds=$MANUAL_DURATION
scenario_repeat=$SCENARIO_REPEAT
before_binary=$BEFORE_BIN
after_binary=$AFTER_BIN
tty=$current_tty
EOF

run_variant before-a "$BEFORE_BIN"
run_variant after "$AFTER_BIN"
run_variant before-b "$BEFORE_BIN"

comparison_file="$OUTPUT_DIR/comparison.txt"
: >"$comparison_file"
comparison_failed=0
for workload in open-close terminal-activity browser-activity manual-normal; do
    for baseline in before-a before-b; do
        {
            echo
            echo "============================================================"
            echo "$workload: $baseline -> after"
            echo "============================================================"
            python3 "$ANALYZER" compare \
                --baseline "$OUTPUT_DIR/$baseline/$workload/perceptual.json" \
                --candidate "$OUTPUT_DIR/after/$workload/perceptual.json"
        } >>"$comparison_file" 2>&1 || comparison_failed=1
    done
done

mkdir -p "$ROOT/target/perceptual-runs"
printf '%s\n' "$OUTPUT_DIR" >"$ROOT/target/perceptual-runs/LATEST"

cat "$comparison_file"
echo
echo "Campaign complete: $OUTPUT_DIR"
echo "Comparison report: $comparison_file"
echo "Return to KDE and ask Codex to analyze target/perceptual-runs/LATEST."

exit "$comparison_failed"

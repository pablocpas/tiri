#!/usr/bin/env bash
# Run a differential campaign across many seeds, in parallel.
#
# A seed's budget is wall-clock, so twenty seeds at 150s is fifty minutes of one core while
# the rest of the machine idles. Nothing about a campaign is shared: every session builds its
# own config, its own temp dir and its own SWAYSOCK keyed by pid, so the seeds are already
# independent processes waiting to be told so.
#
#   tiri-parity/campaign.sh [seeds] [seconds] [jobs]
#
# Domain comes from the environment, same as a single run: PARITY_FUZZ_TILING_ONLY=1 for the
# tree commands alone, PARITY_FUZZ_NO_FLOATING=1 for the whole tiled domain, neither for
# everything.
set -uo pipefail

# The search finds sway crashes on purpose — one is in the ledger as a known reference
# failure — and every one of them is a core dump the desktop offers to report. A campaign of
# twenty seeds turns that into twenty notifications about a compositor the user never started.
ulimit -c 0

SEEDS="${1:-20}"
SECONDS_PER_SEED="${2:-150}"
JOBS="${3:-$(( $(nproc) / 2 ))}"
JOBS=$(( JOBS < 1 ? 1 : JOBS ))

: "${TIRI_PARITY_SWAY:=$HOME/.cache/tiri-parity/sway-build/sway/sway}"
: "${TIRI_PARITY_SWAYMSG:=$HOME/.cache/tiri-parity/sway-build/swaymsg/swaymsg}"
export TIRI_PARITY_SWAY TIRI_PARITY_SWAYMSG

if [ ! -x "$TIRI_PARITY_SWAY" ]; then
    echo "no oracle at $TIRI_PARITY_SWAY — build it with tiri-parity/oracle.sh" >&2
    exit 1
fi

OUT=$(mktemp -d); trap 'rm -rf "$OUT"' EXIT
echo "$SEEDS seeds, ${SECONDS_PER_SEED}s each, $JOBS at a time"

# Build once, so the workers do not race on the same target directory.
cargo test --no-run --lib >/dev/null 2>&1 || { echo "build failed" >&2; exit 1; }

run_seed() {
    local seed=$1
    RUN_PARITY_FUZZ=1 PARITY_FUZZ_SEED="$seed" PARITY_FUZZ_SEEDS=1 \
      PARITY_FUZZ_SECONDS="$SECONDS_PER_SEED" \
      cargo test --lib layout::tests::parity::fuzz::differential_fuzz_against_sway \
      -- --nocapture >"$OUT/$seed" 2>&1
}
export -f run_seed
export OUT SECONDS_PER_SEED

seq 1 "$SEEDS" | xargs -P "$JOBS" -I{} bash -c 'run_seed {}'

clean=0; diverged=0; broke=0
for seed in $(seq 1 "$SEEDS"); do
    f="$OUT/$seed"
    if grep -q "scripts compared against" "$f"; then
        printf "  seed %-4s clean   %s\n" "$seed" "$(grep -oE '^[0-9]+ scripts compared' "$f")"
        clean=$((clean + 1))
    elif grep -q "divergence after" "$f"; then
        printf "  seed %-4s DIVERGED  %s\n" "$seed" "$(grep -oE 'shrunk to [0-9]+ commands' "$f" | head -1)"
        sed -n '/^step [0-9]* — after/,/^$/p' "$f" | head -3 | sed 's/^/           /'
        diverged=$((diverged + 1))
    else
        printf "  seed %-4s HARNESS BROKE\n" "$seed"
        grep -E "panicked|cannot" "$f" | head -1 | sed 's/^/           /'
        broke=$((broke + 1))
    fi
done

echo
echo "$clean clean, $diverged diverged, $broke harness failures"
[ "$diverged" -eq 0 ] && [ "$broke" -eq 0 ]

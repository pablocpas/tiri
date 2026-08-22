#!/usr/bin/env bash
# Build the sway the corpus is recorded against, somewhere it survives a reboot.
#
# The reference is a *release*, named by tag, and built here rather than taken from the
# distribution: what a distribution ships is whatever it happened to package, which on this
# machine is a version behind. A difference from an unreleased tree is not yet a difference
# from sway, and a difference from a superseded release is a bug someone already fixed —
# neither is parity. So the oracle is the newest tag, and `TIRI_PARITY_SWAY_REF` names
# another when the question is which release changed a behaviour.
#
# It lives in ~/.cache rather than /tmp because the first one did not, and emptied on the next
# boot — and because a fuzz campaign that cannot start sway looks exactly like one that found
# nothing, that loss was reported as agreement for a while before anyone noticed.
#
# Fedora ships wlroots 0.19; sway 1.12 wants 0.20, so it is vendored as a subproject rather
# than taken from the system.
set -euo pipefail

ROOT="${TIRI_PARITY_ORACLE_ROOT:-$HOME/.cache/tiri-parity}"
SWAY_REF="${TIRI_PARITY_SWAY_REF:-1.12}"
SWAY_REPO="${TIRI_PARITY_SWAY_REPO:-https://github.com/swaywm/sway.git}"
WLROOTS_TAG="${TIRI_PARITY_WLROOTS_TAG:-0.20.0}"
# A local tree instead of a tag, for testing a patch that is not upstream yet.
SWAY_SRC="${TIRI_PARITY_SWAY_SRC:-}"

mkdir -p "$ROOT"

if [ ! -d "$ROOT/wlroots-$WLROOTS_TAG" ]; then
    git clone --depth 1 --branch "$WLROOTS_TAG" \
        https://gitlab.freedesktop.org/wlroots/wlroots.git "$ROOT/wlroots-$WLROOTS_TAG"
fi

rm -rf "$ROOT/src"
if [ -n "$SWAY_SRC" ]; then
    mkdir -p "$ROOT/src"
    cp -a "$SWAY_SRC/." "$ROOT/src/"
else
    # Cloned rather than copied, and shallow at one tag: the build stamps its version from
    # git, so a tree without history calls itself `-dev` and every fixture it records lies
    # about which sway answered.
    git clone --depth 1 --branch "$SWAY_REF" "$SWAY_REPO" "$ROOT/src"
fi
mkdir -p "$ROOT/src/subprojects"
rm -rf "$ROOT/src/subprojects/wlroots"
cp -a "$ROOT/wlroots-$WLROOTS_TAG" "$ROOT/src/subprojects/wlroots"

rm -rf "$ROOT/sway-build"
meson setup "$ROOT/sway-build" "$ROOT/src" --buildtype=release -Dwlroots:examples=false
ninja -C "$ROOT/sway-build"

cat <<MSG

Built. Point the harness at it:

  export TIRI_PARITY_SWAY=$ROOT/sway-build/sway/sway
  export TIRI_PARITY_SWAYMSG=$ROOT/sway-build/swaymsg/swaymsg

$("$ROOT/sway-build/sway/sway" --version)
MSG

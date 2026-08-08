#!/usr/bin/env bash
# Build the sway the fixtures were recorded against, somewhere it survives a reboot.
#
# The corpus is only comparable against one build: every fixture header names it, and the
# recorder refuses a session it cannot identify. The first oracle lived in /tmp, which
# emptied on the next boot and took the source with it — and because a fuzz campaign that
# cannot start sway looks exactly like one that found nothing, that loss was reported as
# agreement for a while before anyone noticed. Hence this file, and hence ~/.cache.
#
# Fedora ships wlroots 0.19; sway 1.12-dev wants 0.20, so it is vendored as a subproject
# rather than taken from the system.
set -euo pipefail

ROOT="${TIRI_PARITY_ORACLE_ROOT:-$HOME/.cache/tiri-parity}"
SWAY_SRC="${TIRI_PARITY_SWAY_SRC:-$HOME/Documentos/sway-master}"
WLROOTS_TAG="${TIRI_PARITY_WLROOTS_TAG:-0.20.0}"

mkdir -p "$ROOT"

if [ ! -d "$ROOT/wlroots-$WLROOTS_TAG" ]; then
    git clone --depth 1 --branch "$WLROOTS_TAG" \
        https://gitlab.freedesktop.org/wlroots/wlroots.git "$ROOT/wlroots-$WLROOTS_TAG"
fi

rm -rf "$ROOT/src"
mkdir -p "$ROOT/src"
cp -a "$SWAY_SRC/." "$ROOT/src/"
mkdir -p "$ROOT/src/subprojects"
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

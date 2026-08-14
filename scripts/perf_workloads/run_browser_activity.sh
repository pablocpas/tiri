#!/usr/bin/env bash
set -eu

workload_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
profile_dir="$(mktemp -d /tmp/tiri-brave-profile.XXXXXX)"
browser_pid=""

cleanup() {
    trap - EXIT HUP INT TERM
    if [ -n "$browser_pid" ] && kill -0 "$browser_pid" 2>/dev/null; then
        kill "$browser_pid" 2>/dev/null || true
        wait "$browser_pid" 2>/dev/null || true
    fi
    case "$profile_dir" in
        /tmp/tiri-brave-profile.*) rm -rf -- "$profile_dir" ;;
    esac
}
trap cleanup EXIT HUP INT TERM

brave-browser-stable \
    --ozone-platform=wayland \
    --user-data-dir="$profile_dir" \
    --app="file://$workload_dir/browser_activity.html" \
    --no-first-run \
    --no-default-browser-check \
    --disable-background-mode \
    --disable-background-networking \
    --disable-component-update \
    --disable-default-apps \
    --disable-sync \
    --password-store=basic &
browser_pid="$!"
wait "$browser_pid"

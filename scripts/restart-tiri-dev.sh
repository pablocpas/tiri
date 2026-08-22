#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "${script_dir}/.." && pwd)"
release_binary="${repo_dir}/target/release/tiri"
config_root="${XDG_CONFIG_HOME:-${HOME}/.config}"
drop_in_dir="${config_root}/systemd/user/tiri.service.d"
drop_in_file="${drop_in_dir}/50-local-build.conf"
managed_marker="# Managed by scripts/restart-tiri-dev.sh"

usage() {
    printf '%s\n' \
        "Usage: scripts/restart-tiri-dev.sh [--no-build | --restore]" \
        "" \
        "Build tiri in release mode, point the user tiri.service at that build, and" \
        "enqueue a restart. The current Wayland clients will be disconnected." \
        "" \
        "  --no-build  Restart the already-built target/release/tiri." \
        "  --restore   Remove the override and restart the packaged tiri." \
        "  -h, --help  Show this help."
}

mode="build"
case "${1:-}" in
    "") ;;
    --no-build) mode="no-build" ;;
    --restore) mode="restore" ;;
    -h|--help)
        usage
        exit 0
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac

if [[ $# -gt 1 ]]; then
    usage >&2
    exit 2
fi

if ! command -v systemctl >/dev/null 2>&1; then
    printf 'systemctl is required; this script only supports systemd user sessions.\n' >&2
    exit 1
fi

if ! systemctl --user is-active --quiet tiri.service; then
    printf '%s\n' \
        "tiri.service is not active; refusing to start a second compositor." \
        "Run this from the systemd-managed tiri session, or start tiri-session first." >&2
    exit 1
fi

if [[ -e "${drop_in_file}" ]] && ! grep -Fxq -- "${managed_marker}" "${drop_in_file}"; then
    printf 'Refusing to overwrite an unmanaged systemd drop-in: %s\n' "${drop_in_file}" >&2
    exit 1
fi

if [[ "${mode}" == "restore" ]]; then
    if [[ -e "${drop_in_file}" ]]; then
        rm -- "${drop_in_file}"
        rmdir --ignore-fail-on-non-empty -- "${drop_in_dir}"
    fi
    systemctl --user daemon-reload
    printf 'Restored the packaged tiri. Restarting the compositor now.\n'
    systemctl --user restart --no-block tiri.service
    exit 0
fi

if [[ "${mode}" == "build" ]]; then
    cargo build --manifest-path "${repo_dir}/Cargo.toml" --release
fi

if [[ ! -x "${release_binary}" ]]; then
    printf 'Release binary not found or not executable: %s\n' "${release_binary}" >&2
    printf 'Run without --no-build first.\n' >&2
    exit 1
fi

# systemd accepts a quoted executable path. Escape the two characters that are special inside
# its double-quoted syntax so a checkout path with spaces remains valid.
escaped_binary="${release_binary//\\/\\\\}"
escaped_binary="${escaped_binary//\"/\\\"}"

install -d -m 0755 -- "${drop_in_dir}"
temp_drop_in="$(mktemp "${drop_in_dir}/.50-local-build.conf.XXXXXX")"
trap 'rm -f -- "${temp_drop_in}"' EXIT
printf '%s\n[Service]\nExecStart=\nExecStart="%s" --session\n' \
    "${managed_marker}" "${escaped_binary}" > "${temp_drop_in}"
chmod 0644 -- "${temp_drop_in}"
mv -f -- "${temp_drop_in}" "${drop_in_file}"
trap - EXIT

systemctl --user daemon-reload

printf 'Built and selected: %s\n' "${release_binary}"
printf '%s\n' \
    "Restarting the compositor now; open applications will lose their Wayland connection." \
    "Recovery from a TTY: ${repo_dir}/scripts/restart-tiri-dev.sh --restore"
systemctl --user restart --no-block tiri.service

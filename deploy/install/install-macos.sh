#!/usr/bin/env bash
# Superkube installer for macOS (launchd).
#
# Usage:
#   sudo ./install-macos.sh server [--binary <path>] [--db-url <url>]
#   sudo ./install-macos.sh uninstall
#
# What it does:
#   - copies the binary to /usr/local/bin/superkube
#   - creates /usr/local/var/superkube and /var/log/superkube
#   - installs /Library/LaunchDaemons/dev.superkube.server.plist
#   - bootstraps + kickstarts the daemon
#
# macOS notes:
#   - Docker Desktop / Colima / Podman must be running for pods to actually
#     start; the daemon itself will boot regardless.
#   - The `node` mode is rarely useful on macOS (the server already embeds
#     a node agent), so this installer only handles `server`.

set -euo pipefail

LABEL="dev.superkube.server"
PLIST_DEST="/Library/LaunchDaemons/${LABEL}.plist"
BIN_DEST="/usr/local/bin/superkube"
DATA_DIR="/usr/local/var/superkube"
LOG_DIR="/var/log/superkube"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PLIST_SRC="${REPO_ROOT}/deploy/launchd/${LABEL}.plist"

log()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==>\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m==>\033[0m %s\n' "$*" >&2; exit 1; }

require_root() {
    [[ $EUID -eq 0 ]] || die "Must run as root (sudo)."
}

require_macos() {
    [[ "$(uname -s)" == "Darwin" ]] || die "this script targets macOS — use install-linux.sh on Linux."
}

detect_binary() {
    local candidates=(
        "${REPO_ROOT}/superkube"
        "${REPO_ROOT}/target/release/superkube"
    )
    for c in "${candidates[@]}"; do
        if [[ -x "$c" ]]; then echo "$c"; return 0; fi
    done
    return 1
}

cmd_server() {
    local binary=""
    local db_url=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --binary) binary="$2"; shift 2;;
            --db-url) db_url="$2"; shift 2;;
            *) die "unknown flag: $1";;
        esac
    done

    require_macos
    require_root

    if [[ -z "${binary}" ]]; then
        binary="$(detect_binary)" || die "could not auto-detect a superkube binary; pass --binary <path>"
    fi
    [[ -x "${binary}" ]] || die "binary not executable: ${binary}"
    [[ -f "${PLIST_SRC}" ]] || die "missing plist template: ${PLIST_SRC}"

    log "installing binary ${binary} -> ${BIN_DEST}"
    install -m 0755 "${binary}" "${BIN_DEST}"

    log "creating ${DATA_DIR} and ${LOG_DIR}"
    install -d -m 0755 "${DATA_DIR}"
    install -d -m 0755 "${LOG_DIR}"

    log "installing plist -> ${PLIST_DEST}"
    install -m 0644 -o root -g wheel "${PLIST_SRC}" "${PLIST_DEST}"

    if [[ -n "${db_url}" ]]; then
        log "patching DATABASE_URL in plist"
        # /usr/libexec/PlistBuddy is the right tool for this on macOS.
        /usr/libexec/PlistBuddy -c "Set :EnvironmentVariables:DATABASE_URL ${db_url}" "${PLIST_DEST}"
    fi

    # Bootstrap is idempotent — bootout first if already loaded so we pick up plist changes.
    if launchctl print "system/${LABEL}" >/dev/null 2>&1; then
        log "service already loaded; reloading"
        launchctl bootout "system/${LABEL}" || true
    fi
    log "bootstrapping launchd service"
    launchctl bootstrap system "${PLIST_DEST}"
    launchctl enable "system/${LABEL}"
    launchctl kickstart -k "system/${LABEL}"

    log "done. logs:"
    echo "    tail -f ${LOG_DIR}/server.log ${LOG_DIR}/server.err"
    echo "    kubectl --server=http://localhost:6443 get nodes"
}

cmd_uninstall() {
    require_macos
    require_root

    if launchctl print "system/${LABEL}" >/dev/null 2>&1; then
        log "stopping and removing service"
        launchctl bootout "system/${LABEL}" || true
    fi
    [[ -f "${PLIST_DEST}" ]] && rm -f "${PLIST_DEST}"
    warn "leaving ${BIN_DEST}, ${DATA_DIR}, and ${LOG_DIR} in place — remove manually if desired."
    log "uninstalled."
}

[[ $# -ge 1 ]] || die "usage: $0 {server|uninstall} [args...]"
sub="$1"; shift
case "${sub}" in
    server)    cmd_server "$@";;
    uninstall) cmd_uninstall "$@";;
    -h|--help)
        sed -n '2,/^set -euo/p' "$0" | sed -n '/^#/p' | sed 's/^# \{0,1\}//';;
    *) die "unknown subcommand: ${sub}";;
esac

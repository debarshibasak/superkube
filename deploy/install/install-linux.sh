#!/usr/bin/env bash
# Superkube installer for Linux (systemd).
#
# Usage:
#   sudo ./install-linux.sh server [--binary <path>] [--db-url <url>]
#   sudo ./install-linux.sh node   --server <url> [--binary <path>] \
#                                   [--name <hostname>] [--runtime auto|docker|embedded]
#   sudo ./install-linux.sh uninstall [server|node|all]
#
# What it does:
#   - creates user/group `superkube` (system account, no shell)
#   - copies the binary to /usr/local/bin/superkube
#   - creates /var/lib/superkube and /var/log/superkube
#   - writes /etc/superkube/superkube.env
#   - installs the matching systemd unit and runs `systemctl enable --now`
#
# The script is idempotent — re-run it to upgrade the binary or change config.

set -euo pipefail

# -------- defaults -------- #
USER_NAME="superkube"
GROUP_NAME="superkube"
BIN_DEST="/usr/local/bin/superkube"
DATA_DIR="/var/lib/superkube"
LOG_DIR="/var/log/superkube"
ETC_DIR="/etc/superkube"
ENV_FILE="${ETC_DIR}/superkube.env"
UNIT_DIR="/etc/systemd/system"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
DEPLOY_DIR="${REPO_ROOT}/deploy"

# -------- helpers -------- #
log()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==>\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m==>\033[0m %s\n' "$*" >&2; exit 1; }

require_root() {
    [[ $EUID -eq 0 ]] || die "Must run as root (sudo)."
}

require_systemd() {
    command -v systemctl >/dev/null 2>&1 || die "systemd not found — this script targets systemd-managed Linux."
}

detect_binary() {
    # Try, in order: --binary arg, ./superkube, target/release/superkube,
    # target/<linux-triple>/release/superkube under repo root.
    local candidates=(
        "${REPO_ROOT}/superkube"
        "${REPO_ROOT}/target/release/superkube"
        "${REPO_ROOT}/target/x86_64-unknown-linux-gnu/release/superkube"
        "${REPO_ROOT}/target/x86_64-unknown-linux-musl/release/superkube"
        "${REPO_ROOT}/target/aarch64-unknown-linux-gnu/release/superkube"
        "${REPO_ROOT}/target/aarch64-unknown-linux-musl/release/superkube"
    )
    for c in "${candidates[@]}"; do
        if [[ -x "$c" ]]; then echo "$c"; return 0; fi
    done
    return 1
}

ensure_user() {
    if ! getent group "${GROUP_NAME}" >/dev/null; then
        log "creating group ${GROUP_NAME}"
        groupadd --system "${GROUP_NAME}"
    fi
    if ! id -u "${USER_NAME}" >/dev/null 2>&1; then
        log "creating user ${USER_NAME}"
        useradd --system --gid "${GROUP_NAME}" --home-dir "${DATA_DIR}" \
                --shell /usr/sbin/nologin "${USER_NAME}"
    fi
    # Add to docker group if it exists, so the agent can read docker.sock.
    if getent group docker >/dev/null; then
        log "adding ${USER_NAME} to docker group"
        usermod -aG docker "${USER_NAME}"
    fi
}

ensure_dirs() {
    install -d -m 0755 -o "${USER_NAME}" -g "${GROUP_NAME}" "${DATA_DIR}"
    install -d -m 0755 -o "${USER_NAME}" -g "${GROUP_NAME}" "${LOG_DIR}"
    install -d -m 0755 "${ETC_DIR}"
}

install_binary() {
    local src="$1"
    log "installing binary ${src} -> ${BIN_DEST}"
    install -m 0755 "${src}" "${BIN_DEST}"
}

write_env_file() {
    local mode="$1"  # server|node
    if [[ -f "${ENV_FILE}" ]]; then
        log "env file already exists at ${ENV_FILE} — leaving as-is (edit manually if needed)"
        return
    fi
    log "writing ${ENV_FILE}"
    local tmpl="${DEPLOY_DIR}/systemd/superkube.env.example"
    if [[ -f "${tmpl}" ]]; then
        install -m 0644 "${tmpl}" "${ENV_FILE}"
    else
        # Inline fallback if the example file isn't shipped alongside.
        cat > "${ENV_FILE}" <<EOF
DATABASE_URL=sqlite://${DATA_DIR}/superkube.db
SUPERKUBE_SERVER=http://127.0.0.1:6443
SUPERKUBE_RUNTIME=auto
EOF
        chmod 0644 "${ENV_FILE}"
    fi

    if [[ "${mode}" == "node" ]]; then
        if [[ -n "${SERVER_URL:-}" ]]; then
            sed -i "s|^SUPERKUBE_SERVER=.*|SUPERKUBE_SERVER=${SERVER_URL}|" "${ENV_FILE}"
        fi
        if [[ -n "${RUNTIME:-}" ]]; then
            sed -i "s|^SUPERKUBE_RUNTIME=.*|SUPERKUBE_RUNTIME=${RUNTIME}|" "${ENV_FILE}"
        fi
    fi

    if [[ "${mode}" == "server" && -n "${DB_URL:-}" ]]; then
        sed -i "s|^DATABASE_URL=.*|DATABASE_URL=${DB_URL}|" "${ENV_FILE}"
    fi
}

install_unit() {
    local unit="$1"  # superkube-server.service or superkube-node.service
    local src="${DEPLOY_DIR}/systemd/${unit}"
    [[ -f "${src}" ]] || die "missing unit file: ${src}"
    log "installing ${unit} -> ${UNIT_DIR}"
    install -m 0644 "${src}" "${UNIT_DIR}/${unit}"
}

start_unit() {
    local unit="$1"
    log "reloading systemd"
    systemctl daemon-reload
    log "enabling and starting ${unit}"
    systemctl enable --now "${unit}"
    sleep 1
    systemctl --no-pager --full status "${unit}" || true
}

# -------- subcommands -------- #
cmd_server() {
    local binary=""
    DB_URL=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --binary)  binary="$2"; shift 2;;
            --db-url)  DB_URL="$2"; shift 2;;
            *) die "unknown server flag: $1";;
        esac
    done

    require_root
    require_systemd

    if [[ -z "${binary}" ]]; then
        binary="$(detect_binary)" || die "could not auto-detect a superkube binary; pass --binary <path>"
    fi
    [[ -x "${binary}" ]] || die "binary not executable: ${binary}"

    ensure_user
    ensure_dirs
    install_binary "${binary}"
    write_env_file server
    install_unit superkube-server.service
    start_unit superkube-server.service

    log "done. API on :6443. Try:"
    echo "    kubectl --server=http://localhost:6443 get nodes"
}

cmd_node() {
    local binary=""
    SERVER_URL=""
    NODE_NAME=""
    RUNTIME=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --binary)  binary="$2";    shift 2;;
            --server)  SERVER_URL="$2"; shift 2;;
            --name)    NODE_NAME="$2"; shift 2;;
            --runtime) RUNTIME="$2";   shift 2;;
            *) die "unknown node flag: $1";;
        esac
    done

    require_root
    require_systemd
    [[ -n "${SERVER_URL}" ]] || die "--server <url> is required for node install"

    if [[ -z "${binary}" ]]; then
        binary="$(detect_binary)" || die "could not auto-detect a superkube binary; pass --binary <path>"
    fi
    [[ -x "${binary}" ]] || die "binary not executable: ${binary}"

    ensure_user
    ensure_dirs
    install_binary "${binary}"
    write_env_file node
    if [[ -n "${NODE_NAME}" ]]; then
        if grep -q '^SUPERKUBE_NODE_NAME=' "${ENV_FILE}"; then
            sed -i "s|^SUPERKUBE_NODE_NAME=.*|SUPERKUBE_NODE_NAME=${NODE_NAME}|" "${ENV_FILE}"
        else
            printf '\nSUPERKUBE_NODE_NAME=%s\n' "${NODE_NAME}" >> "${ENV_FILE}"
        fi
    fi
    install_unit superkube-node.service
    start_unit superkube-node.service

    log "done. Node agent connecting to ${SERVER_URL}."
}

cmd_uninstall() {
    require_root
    local target="${1:-all}"
    case "${target}" in
        server|node|all) ;;
        *) die "uninstall target must be server|node|all";;
    esac

    for u in superkube-server.service superkube-node.service; do
        if [[ "${target}" == "all" || "${u}" == "superkube-${target}.service" ]]; then
            if systemctl list-unit-files | grep -q "^${u}"; then
                log "stopping and disabling ${u}"
                systemctl disable --now "${u}" || true
                rm -f "${UNIT_DIR}/${u}"
            fi
        fi
    done
    systemctl daemon-reload

    if [[ "${target}" == "all" ]]; then
        warn "leaving ${BIN_DEST}, ${DATA_DIR}, ${ETC_DIR}, and user '${USER_NAME}' in place — remove manually if desired."
    fi
    log "uninstalled."
}

# -------- entry -------- #
[[ $# -ge 1 ]] || die "usage: $0 {server|node|uninstall} [args...]"
sub="$1"; shift
case "${sub}" in
    server)    cmd_server "$@";;
    node)      cmd_node "$@";;
    uninstall) cmd_uninstall "$@";;
    -h|--help)
        sed -n '2,/^set -euo/p' "$0" | sed -n '/^#/p' | sed 's/^# \{0,1\}//';;
    *) die "unknown subcommand: ${sub}";;
esac

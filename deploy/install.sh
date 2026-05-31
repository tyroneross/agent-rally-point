#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# Install / uninstall the cockpitd LaunchAgent on an always-on Mac.
# Usage:
#   deploy/install.sh install     # build, copy binary, render+load LaunchAgent
#   deploy/install.sh uninstall   # unload + remove LaunchAgent
#   deploy/install.sh status      # show launchctl state
#
# TAG:UNTESTED — the launchctl bootstrap/bootout paths require a real login
# session on the target Mac and are not exercised by the autonomous build.
set -euo pipefail

LABEL="ai.rosslabs.cockpitd"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLIST_SRC="${REPO_ROOT}/deploy/${LABEL}.plist"
LA_DIR="${HOME}/Library/LaunchAgents"
PLIST_DST="${LA_DIR}/${LABEL}.plist"
BIN_DST="${HOME}/.local/bin/cockpitd"
LOG_DIR="${HOME}/Library/Logs/cockpitd"
DOMAIN="gui/$(id -u)"

cmd_install() {
    echo ">> building cockpitd (release)"
    (cd "${REPO_ROOT}" && cargo build --release -p cockpitd)
    mkdir -p "$(dirname "${BIN_DST}")" "${LA_DIR}" "${LOG_DIR}"
    cp "${REPO_ROOT}/target/release/cockpitd" "${BIN_DST}"

    echo ">> rendering LaunchAgent -> ${PLIST_DST}"
    sed -e "s|__COCKPITD_BIN__|${BIN_DST}|g" \
        -e "s|__LOG_DIR__|${LOG_DIR}|g" \
        "${PLIST_SRC}" > "${PLIST_DST}"
    plutil -lint "${PLIST_DST}"

    echo ">> loading via launchctl (${DOMAIN})"
    launchctl bootout "${DOMAIN}/${LABEL}" 2>/dev/null || true
    launchctl bootstrap "${DOMAIN}" "${PLIST_DST}"
    launchctl enable "${DOMAIN}/${LABEL}"
    echo ">> installed. tail logs: tail -f ${LOG_DIR}/cockpitd.err.log"
}

cmd_uninstall() {
    launchctl bootout "${DOMAIN}/${LABEL}" 2>/dev/null || true
    rm -f "${PLIST_DST}"
    echo ">> uninstalled ${LABEL}"
}

cmd_status() {
    launchctl print "${DOMAIN}/${LABEL}" 2>/dev/null || echo "not loaded"
}

case "${1:-}" in
    install)   cmd_install ;;
    uninstall) cmd_uninstall ;;
    status)    cmd_status ;;
    *) echo "usage: $0 {install|uninstall|status}" >&2; exit 2 ;;
esac

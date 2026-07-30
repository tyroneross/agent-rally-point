#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# build-codex-artifact.sh — compatibility wrapper for the canonical host generator.
#
# WHY: Existing callers use this script, while all host output is now generated
# from config/host-integrations.json by generate_host_surfaces.py. Keep one
# implementation and preserve RALLY_CODEX_DEST for scratch parity checks.
#
# SEC-003: `dest` remains overridable via RALLY_CODEX_DEST. The generator
# refuses broad targets and requires the destination basename `.codex-plugin`.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd -P)"
dest="${RALLY_CODEX_DEST:-$repo_root/plugins/codex/.codex-plugin}"

exec python3 "$repo_root/scripts/generate_host_surfaces.py" \
  --root "$repo_root" \
  --artifact-only \
  --artifact-dest "$dest"

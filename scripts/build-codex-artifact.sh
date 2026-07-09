#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# build-codex-artifact.sh — regenerate the self-contained Codex plugin artifact.
#
# WHY: Codex resolves a marketplace plugin from `<source.path>/.codex-plugin/
# plugin.json` and rejects a root ("./") path, so the Codex plugin must live in
# a dedicated SUBDIR that Codex can copy wholesale on install. The canonical
# source is the repo-root `.codex-plugin/`; this script mirrors it into
# `plugins/codex/.codex-plugin/` so there is ONE source of truth (root) and the
# artifact is reproducible, not hand-maintained. Re-run after editing any
# `.codex-plugin/` skill or manifest. `.agents/plugins/marketplace.json` points
# its plugin source at `./plugins/codex`.
#
# SEC-003: `dest` is overridable via RALLY_CODEX_DEST so
# scripts/check-release-parity.sh can invoke THIS script into a scratch dir
# to verify freshness — single-sourcing the copy semantics (`-L`,
# `.DS_Store` strip) instead of re-implementing them and risking drift
# between the two scripts. Default is UNCHANGED for every normal caller.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd -P)"
src="$repo_root/.codex-plugin"
dest="${RALLY_CODEX_DEST:-$repo_root/plugins/codex/.codex-plugin}"

[ -d "$src" ] || { echo "error: $src not found" >&2; exit 1; }

rm -rf "$dest"
mkdir -p "$dest"
# Copy plugin.json + skills/ (everything Codex needs to run the plugin).
# -L DEREFERENCES symlinks: the root .codex-plugin/skills/* are symlinks into
# the repo's main skills/ (DRY), but the artifact must be SELF-CONTAINED — Codex
# copies the subdir wholesale on install, so relative symlinks would dangle and
# ship empty skills. Follow them to materialize real files.
cp -RL "$src/." "$dest/"
# Strip macOS cruft so it never ships in the artifact.
find "$dest" -name '.DS_Store' -delete 2>/dev/null || true

echo "Built Codex artifact: $dest (mirrors $src)"

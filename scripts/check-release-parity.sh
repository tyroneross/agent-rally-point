#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# check-release-parity.sh — fail if the CLI version and the bundled Codex
# artifact have drifted from their canonical sources.
#
# Canonical CLI version: `crates/rally-cli/Cargo.toml` [package] `version`.
# (NOT the root `Cargo.toml` [workspace.package] table — this workspace has
# no `version` key there; every crate pins its own version explicitly, and
# `rally-cli` is the crate that ships as the released `rally` binary, so it
# is the canonical source of truth for release version numbers.)
#
# Checks, both required to pass:
#   1. Every one of the following has a `version` field equal to the
#      canonical CLI version:
#        .claude-plugin/plugin.json
#        .codex-plugin/plugin.json
#        plugins/codex/.codex-plugin/plugin.json
#        .agents/plugins/marketplace.json
#   2. plugins/codex/.codex-plugin/ is byte-identical to what
#      scripts/build-codex-artifact.sh would (re)generate from the canonical
#      .codex-plugin/ source right now — i.e. the committed artifact is not
#      stale. Verified by INVOKING that script (RALLY_CODEX_DEST=<scratch>)
#      rather than re-implementing its copy semantics here — single source
#      of truth (SEC-003), no risk of the two scripts' `cp` flags drifting
#      apart. The scratch dest means this never mutates the committed tree.
#   3. plugins/codex/.codex-plugin/ contains NO symlinks (SEC-004),
#      independent of what the (2) content diff reports. Codex installs by
#      copying the directory wholesale — a committed symlink ships as a
#      dangling link on the installer's machine even if its target's
#      CONTENT happens to diff clean here.
#
# Exit 0 only when both hold. JSON is parsed with `python3 -c` (portable, no
# jq dependency) to match the rest of this repo's tooling (see
# tests/hooks/test_install_rally_hooks.sh).
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

fail=0

cli_cargo_toml="crates/rally-cli/Cargo.toml"
if [ ! -f "$cli_cargo_toml" ]; then
  echo "check-release-parity: FAILED — $cli_cargo_toml not found" >&2
  exit 1
fi

cli_version=$(python3 -c '
import re, sys

path = sys.argv[1]
with open(path, encoding="utf-8") as fh:
    text = fh.read()

m = re.search(r"(?m)^\[package\]\s*$", text)
if not m:
    sys.exit(f"{path}: no [package] table found")
rest = text[m.end():]
# stop at the next top-level [section] so we do not accidentally match a
# version-looking key belonging to a later table
next_section = re.search(r"(?m)^\[", rest)
if next_section:
    rest = rest[: next_section.start()]
vm = re.search(r"(?m)^version\s*=\s*\"([^\"]+)\"", rest)
if not vm:
    sys.exit(f"{path}: no version = \"...\" under [package]")
print(vm.group(1))
' "$cli_cargo_toml")

echo "check-release-parity: canonical CLI version = $cli_version ($cli_cargo_toml)" >&2

check_json_version() {
  path="$1"
  # "optional" = an ABSENT version key is compliant; a PRESENT one must match.
  optional="${2:-}"
  if [ ! -f "$path" ]; then
    echo "check-release-parity: MISSING $path" >&2
    fail=1
    return
  fi
  found=$(python3 -c '
import json, sys
try:
    data = json.load(open(sys.argv[1], encoding="utf-8"))
except Exception as exc:
    sys.exit(f"invalid JSON: {exc}")
print(data.get("version", ""))
' "$path")
  if [ -z "$found" ] && [ -n "$optional" ]; then
    echo "check-release-parity: $path omits version (git-source policy, aeae79e) — OK" >&2
    return
  fi
  if [ "$found" != "$cli_version" ]; then
    echo "check-release-parity: MISMATCH $path — expected $cli_version, found ${found:-<missing>}" >&2
    fail=1
  fi
}

# Plugin manifests install from a git source, where the host resolves the
# revision itself and a hardcoded version only goes stale. aeae79e removed the
# key from all three on purpose ("track via git tags + marketplace metadata").
# This gate was not updated to match, so it failed on every push after that
# commit. Absent is now compliant; a version that IS present must still match
# the CLI, which is what keeps a stale number from shipping.
check_json_version ".claude-plugin/plugin.json" optional
check_json_version ".codex-plugin/plugin.json" optional
check_json_version "plugins/codex/.codex-plugin/plugin.json" optional
# The marketplace manifest is where the version is tracked, so it stays strict.
check_json_version ".agents/plugins/marketplace.json"

# --- artifact freshness -----------------------------------------------
scratch=$(mktemp -d "${TMPDIR:-/tmp}/rally-parity-XXXXXX")
trap 'rm -rf "$scratch"' EXIT

if [ -d ".codex-plugin" ]; then
  fresh="$scratch/fresh-codex-plugin"
  # SEC-003: generate the fresh comparison artifact by INVOKING the real
  # builder (single source of truth for the copy semantics: -L dereference,
  # .DS_Store strip) rather than re-implementing them here. RALLY_CODEX_DEST
  # redirects the builder's output into a scratch dir, so this never
  # mutates the committed tree.
  RALLY_CODEX_DEST="$fresh" bash scripts/build-codex-artifact.sh >&2

  if [ ! -d "plugins/codex/.codex-plugin" ]; then
    echo "check-release-parity: MISSING plugins/codex/.codex-plugin/ — run scripts/build-codex-artifact.sh" >&2
    fail=1
  else
    diff_output=$(diff -rq "$fresh" "plugins/codex/.codex-plugin" 2>&1 || true)
    if [ -n "$diff_output" ]; then
      echo "check-release-parity: STALE ARTIFACT — plugins/codex/.codex-plugin/ differs from a fresh regeneration of .codex-plugin/. Run scripts/build-codex-artifact.sh." >&2
      printf '%s\n' "$diff_output" >&2
      fail=1
    else
      echo "check-release-parity: bundled Codex artifact is current" >&2
    fi

    # SEC-004: a committed symlink under the artifact ships as a dangling
    # link on install (Codex copies the directory wholesale, it does not
    # dereference). Check independently of the content diff above.
    symlinks=$(find "plugins/codex/.codex-plugin" -type l 2>/dev/null || true)
    if [ -n "$symlinks" ]; then
      echo "check-release-parity: SYMLINKS FOUND in plugins/codex/.codex-plugin/ — the artifact must be self-contained (real files only). Offending path(s):" >&2
      printf '%s\n' "$symlinks" >&2
      fail=1
    fi
  fi
else
  echo "check-release-parity: MISSING .codex-plugin/ (canonical source) — cannot verify artifact freshness" >&2
  fail=1
fi

if [ "$fail" != "0" ]; then
  echo "check-release-parity: FAILED" >&2
  exit 1
fi

echo "check-release-parity: all versions aligned at $cli_version, artifact current ✅" >&2

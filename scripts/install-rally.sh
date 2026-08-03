#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# install-rally.sh — the only way the rally CLI gets installed.
#
# WHY THIS EXISTS (ARP-001). Coordination hooks used to provision the binary on
# SessionStart. That meant opening or trusting the repo could download an
# executable, mark it executable, run it, and write it into your home directory
# before you ran a single line of project code. A malicious commit, a
# compromised release, or a compromised maintainer account got host code
# execution for free. All of that moved here, behind a human.
#
# This installer is FAIL-CLOSED. Being offline, missing `gh`, or failing a
# verification step is a hard stop with a printed reason. It never degrades to
# an unverified download.
#
# Verification on the download path, both mandatory, both before the file is
# made executable:
#   1. SHA256 against the release's published <asset>.sha256.
#   2. `gh attestation verify` against the sigstore build-provenance attestation
#      published by .github/workflows/release.yml. This is the independent
#      authority: the checksum lives on the same GitHub release as the binary,
#      so it alone cannot catch a compromised release.
#
# Usage:
#   scripts/install-rally.sh              # ask, then install (verified release)
#   scripts/install-rally.sh --yes        # no prompt (CI / scripted)
#   scripts/install-rally.sh --dry-run    # print the plan, write nothing
#   scripts/install-rally.sh --source     # build from this checkout with cargo
#   scripts/install-rally.sh --help
#
# Exit codes: 0 installed (or dry run) · 1 refused/failed · 2 bad arguments.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd -P)"
ENGINE="$REPO_ROOT/hooks/ensure-rally-binary.sh"
GH_REPO="tyroneross/agent-rally-point"
LOCAL_BIN="${HOME:-/nonexistent}/.local/bin"
LOCAL_RALLY="$LOCAL_BIN/rally"
CACHE_DIR="${XDG_CACHE_HOME:-${HOME:-/nonexistent}/.cache}/rally"
STATE_FILE="$CACHE_DIR/provision.json"

ASSUME_YES=0
DRY_RUN=0
MODE="release"

usage() {
  sed -n '5,32p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    -y|--yes)      ASSUME_YES=1; shift ;;
    -n|--dry-run)  DRY_RUN=1; shift ;;
    --source)      MODE="source"; shift ;;
    -h|--help)     usage; exit 0 ;;
    *) printf 'install-rally: unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

say()  { printf '%s\n' "$*"; }
fail() { printf 'install-rally: %s\n' "$*" >&2; exit 1; }

if [ -z "${HOME:-}" ]; then
  fail "HOME is not set. There is nowhere safe to install."
fi
if [ ! -x "$ENGINE" ]; then
  fail "provisioning engine missing at $ENGINE. Run this from a checkout of $GH_REPO."
fi

# --- host triple ------------------------------------------------------------
_uname_s="$(uname -s 2>/dev/null || echo unknown)"
_uname_m="$(uname -m 2>/dev/null || echo unknown)"
case "${_uname_s}:${_uname_m}" in
  Darwin:arm64)               HOST_TRIPLE="aarch64-apple-darwin"     ;;
  Darwin:x86_64)              HOST_TRIPLE="x86_64-apple-darwin"      ;;
  Linux:x86_64)               HOST_TRIPLE="x86_64-unknown-linux-gnu" ;;
  Linux:aarch64|Linux:arm64)  HOST_TRIPLE="aarch64-unknown-linux-gnu";;
  *)                          HOST_TRIPLE=""                          ;;
esac

# --- what can actually run here --------------------------------------------
HAVE_CURL=0; command -v curl  >/dev/null 2>&1 && HAVE_CURL=1
HAVE_GH=0;   command -v gh    >/dev/null 2>&1 && HAVE_GH=1
HAVE_CARGO=0;command -v cargo >/dev/null 2>&1 && HAVE_CARGO=1
HAVE_SUM=0
if command -v shasum >/dev/null 2>&1 || command -v sha256sum >/dev/null 2>&1; then HAVE_SUM=1; fi

# --- the plan, in plain language -------------------------------------------
say "install-rally — installs the rally CLI for Agent Rally Point."
say ""
say "This is the only step that installs software. Hooks never do it."
say ""
if [ "$MODE" = "release" ]; then
  say "Plan: download a prebuilt release binary and verify it twice."
  say "  source     https://github.com/$GH_REPO/releases (asset rally-${HOST_TRIPLE:-<unsupported-host>})"
  say "  check 1    SHA256 against the published <asset>.sha256"
  say "  check 2    gh attestation verify --repo $GH_REPO (sigstore build provenance)"
  say "  installs   $LOCAL_RALLY"
  say "  records    $STATE_FILE"
  say ""
  say "Both checks run before the file is made executable. If either fails, or if"
  say "either cannot run, nothing is installed and this exits non-zero."
else
  say "Plan: build from the source in this checkout."
  say "  source     $REPO_ROOT/crates/rally-cli"
  say "  builds     cargo install --path crates/rally-cli --root ${HOME}/.local"
  say "  installs   $LOCAL_RALLY"
  say ""
  say "No download, no signature check. You are trusting the source you have"
  say "checked out. Confirm the commit is what you expect before continuing."
fi
say ""

# --- preflight: refuse now, not halfway through -----------------------------
if [ "$MODE" = "release" ]; then
  [ -n "$HOST_TRIPLE" ] || fail "no release binary is published for ${_uname_s}/${_uname_m}. Build from source instead: $0 --source"
  [ "$HAVE_CURL" = "1" ] || fail "curl is not installed, so the release cannot be fetched. Install curl, or build from source: $0 --source"
  [ "$HAVE_SUM"  = "1" ] || fail "neither shasum nor sha256sum is installed, so the checksum cannot be verified. Install one, or build from source: $0 --source"
  if [ "$HAVE_GH" != "1" ]; then
    printf '%s\n' \
      "install-rally: the GitHub CLI (gh) is not installed, so the build-provenance" \
      "attestation cannot be verified client-side. Refusing the download path — an" \
      "unverified binary is exactly the risk this installer exists to remove." \
      "" \
      "Choose one:" \
      "  1. Install gh (https://cli.github.com), authenticate, and re-run this." \
      "  2. Build from the source you already have:  $0 --source" >&2
    exit 1
  fi
else
  [ "$HAVE_CARGO" = "1" ] || fail "cargo is not installed, so there is nothing to build with. Install Rust (https://rustup.rs), or use the verified release path: $0"
  [ -d "$REPO_ROOT/crates/rally-cli" ] || fail "no source at $REPO_ROOT/crates/rally-cli. Run this from a checkout of $GH_REPO."
fi

if [ "$DRY_RUN" = "1" ]; then
  say "Dry run: nothing was downloaded, built, or written."
  exit 0
fi

# --- explicit confirmation --------------------------------------------------
if [ "$ASSUME_YES" != "1" ]; then
  if [ ! -t 0 ]; then
    fail "no terminal to ask on. Re-run with --yes if you mean it."
  fi
  printf 'Install rally to %s now? [y/N] ' "$LOCAL_RALLY"
  read -r _reply || _reply=""
  case "$(printf '%s' "$_reply" | tr '[:upper:]' '[:lower:]')" in
    y|yes) ;;
    *) say "Cancelled. Nothing was installed."; exit 1 ;;
  esac
fi

# --- do it ------------------------------------------------------------------
# A stale "ok" record would make the engine exit early and report a success it
# did not perform on this run. Clear it so the outcome below is this run.
rm -f "$STATE_FILE" 2>/dev/null || true

say ""
say "Working..."
if [ "$MODE" = "source" ]; then
  # Force the source path by hiding curl from the engine is fragile; call cargo
  # directly instead. Same target dir the engine uses.
  if ! cargo install --path "$REPO_ROOT/crates/rally-cli" --root "$HOME/.local" --quiet; then
    fail "cargo install failed. Nothing was installed."
  fi
  [ -x "$LOCAL_RALLY" ] || fail "cargo reported success but $LOCAL_RALLY is missing."
  say ""
  say "Installed $LOCAL_RALLY"
  say "  built from   $REPO_ROOT/crates/rally-cli"
  say "  verified     nothing — this is a local source build, not a signed artifact"
  say "  NOT verified signature, provenance, checksum"
else
  # RALLY_EXPLICIT_INSTALL=1 is the engine's ARP-001 gate. Only this line sets it.
  RALLY_EXPLICIT_INSTALL=1 "$ENGINE" "$REPO_ROOT" || true

  _method=""; _result=""; _hint=""
  if [ -f "$STATE_FILE" ]; then
    _method="$(grep -o '"method":"[^"]*"' "$STATE_FILE" 2>/dev/null | cut -d'"' -f4 || true)"
    _result="$(grep -o '"result":"[^"]*"' "$STATE_FILE" 2>/dev/null | cut -d'"' -f4 || true)"
    _hint="$(grep -o '"hint":"[^"]*"' "$STATE_FILE" 2>/dev/null | cut -d'"' -f4 || true)"
  fi

  say ""
  case "$_method" in
    downloaded)
      say "Installed $LOCAL_RALLY"
      say "  source       GitHub release asset rally-$HOST_TRIPLE"
      say "  verified     SHA256 against the published <asset>.sha256"
      say "  verified     build provenance via gh attestation verify --repo $GH_REPO"
      say "  NOT verified nothing else — no runtime sandbox, no reproducible-build check"
      ;;
    present)
      say "Already installed and working: $LOCAL_RALLY"
      say "  verified     nothing on this run — an existing binary was found and answered"
      say "  To force a fresh verified download, remove it first: rm -f $LOCAL_RALLY"
      ;;
    source)
      say "Installed $LOCAL_RALLY"
      say "  built from   $REPO_ROOT/crates/rally-cli (the download path was rejected)"
      say "  verified     nothing — local source build, not a signed artifact"
      [ -n "$_hint" ] && say "  why no download: $_hint"
      ;;
    download-rejected)
      say "REFUSED. Nothing was installed."
      [ -n "$_hint" ] && say "  reason: $_hint"
      say "  Build from the source you have instead: $0 --source"
      exit 1
      ;;
    *)
      say "Nothing was installed (result: ${_result:-unknown}, method: ${_method:-none})."
      [ -n "$_hint" ] && say "  reason: $_hint"
      say "  Build from the source you have instead: $0 --source"
      exit 1
      ;;
  esac
fi

say ""
case ":${PATH}:" in
  *":$LOCAL_BIN:"*) ;;
  *) say "Note: $LOCAL_BIN is not on your PATH. Add it, or call $LOCAL_RALLY directly." ;;
esac
say "Next: cd into a repo and run \`rally init\`."
exit 0

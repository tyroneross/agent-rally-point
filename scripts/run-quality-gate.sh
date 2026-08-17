#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# run-quality-gate.sh — the SINGLE shared quality gate for this repo.
#
# Invoked by BOTH `.githooks/pre-push` (once per pushed SHA, inside a clean
# DETACHED worktree at that SHA) and CI (`.github/workflows/ci.yml`), always
# against the CURRENT working directory — the caller is responsible for
# `cd`-ing into the tree to validate before running this script. Keeping the
# gate logic in exactly one place means local pre-push and CI can never
# silently diverge (previously this was duplicated inline in pre-push).
#
# Steps: fmt --check (workspace-wide — safe here because the caller always
# hands us a CLEAN checkout, unlike the old pre-push which had to scope fmt
# to just the pushed .rs files to avoid tripping over a shared dirty tree),
# clippy -D warnings, tests (`cargo nextest` if installed, else serialized
# `cargo test`), doctests, and dependency-changed-gated `cargo audit` /
# `cargo deny` (fail-soft when the optional tools aren't installed).
#
# Toolchain pin: set RALLY_QG_TOOLCHAIN to override; defaults to the repo pin
# below (kept in lockstep with rust-toolchain.toml). Falls back to the
# default `cargo` on PATH if the pinned toolchain isn't installed via rustup.
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

test_mode="${RALLY_QG_TEST_MODE:-auto}"
case "$test_mode" in
  auto|serial) ;;
  *)
    printf 'quality-gate: RALLY_QG_TEST_MODE must be auto or serial, found %q\n' "$test_mode" >&2
    exit 2
    ;;
esac

TC="${RALLY_QG_TOOLCHAIN:-1.95.0}"
if [ -n "$TC" ] && command -v rustup >/dev/null 2>&1 && rustup toolchain list 2>/dev/null | grep -q "^$TC"; then
  CARGO="rustup run $TC cargo"
else
  CARGO="cargo"
  if [ -n "$TC" ]; then
    echo "quality-gate: toolchain $TC not found via rustup; using default cargo (fmt may differ)" >&2
  fi
fi

echo "quality-gate: fmt --check (workspace)…" >&2
# shellcheck disable=SC2086 # $CARGO is an intentional word-split command prefix ("rustup run X cargo" or "cargo")
$CARGO fmt --all --check

echo "quality-gate: clippy (-D warnings)…" >&2
# shellcheck disable=SC2086
$CARGO clippy --workspace --all-targets -- -D warnings

if [ "$test_mode" = "serial" ]; then
  echo "quality-gate: tests (serialized cargo test; RALLY_QG_TEST_MODE=serial)…" >&2
  # shellcheck disable=SC2086
  $CARGO test --workspace -- --test-threads=1
elif command -v cargo-nextest >/dev/null 2>&1; then
  echo "quality-gate: tests (cargo nextest)…" >&2
  # shellcheck disable=SC2086
  $CARGO nextest run --workspace
else
  echo "quality-gate: cargo-nextest not installed; using serialized cargo test fallback" >&2
  echo "quality-gate: install faster gate with: cargo install cargo-nextest --locked" >&2
  # shellcheck disable=SC2086
  $CARGO test --workspace -- --test-threads=1
fi

echo "quality-gate: doctests…" >&2
# shellcheck disable=SC2086
$CARGO test --workspace --doc

# Supply-chain gates: only when dependencies actually changed (fast common
# path), and only if the tools are installed (fail-soft — a missing optional
# tool must not block a legitimate push/CI run; note it instead). We diff
# against the immediate parent commit, which is meaningful here because the
# caller always hands us a single, specific, clean commit to validate (a
# pushed SHA in a detached worktree, or a CI checkout) rather than an
# open-ended branch range. No parent (shallow clone / repo root commit) is
# treated conservatively as "changed" so the gate still runs at least once.
dep_changed=1
if git rev-parse --verify --quiet HEAD~1 >/dev/null 2>&1; then
  if git diff --quiet HEAD~1 HEAD -- Cargo.lock Cargo.toml '**/Cargo.toml' deny.toml .cargo/audit.toml 2>/dev/null; then
    dep_changed=0
  fi
fi
if [ "$dep_changed" = "1" ]; then
  if command -v cargo-audit >/dev/null 2>&1; then
    echo "quality-gate: deps changed — cargo audit…" >&2
    cargo audit
  else
    echo "quality-gate: deps changed but cargo-audit not installed — SKIPPED (install: cargo install cargo-audit)" >&2
  fi
  if command -v cargo-deny >/dev/null 2>&1; then
    echo "quality-gate: deps changed — cargo deny…" >&2
    cargo deny check --hide-inclusion-graph
  else
    echo "quality-gate: deps changed but cargo-deny not installed — SKIPPED (install: cargo install cargo-deny)" >&2
  fi
fi

echo "quality-gate: all checks green ✅" >&2

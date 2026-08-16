#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# release-readiness.sh — one repeatable release-candidate preflight.
#
# Default mode is repo-only and read-only: it checks version/generated-surface
# parity plus whitespace. `--fix-generated` is the sole repair mode, and only
# regenerates deterministic repository surfaces. It never tags, pushes,
# publishes, changes installed plugins, or contacts GitHub Packages.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/release-readiness.sh [--check | --fix-generated] [--full] [--tag vX.Y.Z]

  --check            Run the read-only candidate checks (default).
  --fix-generated    Regenerate deterministic repository host surfaces, then check.
  --full             Also run Rust quality, pre-push hook, packaged workflow,
                     and workflow-syntax gates from one unchanged candidate.
  --tag TAG          Verify an existing release tag and its versioned changelog entry.

This command never creates tags, pushes, publishes releases/packages, or updates
installed host plugins. Use scripts/sync_host_integrations.py --apply only after
an explicit post-release decision.
EOF
}

fix_generated=0
check_requested=0
full=0
release_tag=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --check)
      check_requested=1
      ;;
    --fix-generated)
      fix_generated=1
      ;;
    --full)
      full=1
      ;;
    --tag)
      if [ "$#" -lt 2 ]; then
        echo "release-readiness: --tag requires vX.Y.Z" >&2
        exit 2
      fi
      release_tag="$2"
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "release-readiness: unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

if [ -n "$release_tag" ] && ! [[ "$release_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "release-readiness: --tag must use vX.Y.Z, found $release_tag" >&2
  exit 2
fi
if [ "$check_requested" -eq 1 ] && [ "$fix_generated" -eq 1 ]; then
  echo "release-readiness: --check and --fix-generated are mutually exclusive" >&2
  exit 2
fi
if [ "$fix_generated" -eq 1 ] && [ -n "$release_tag" ]; then
  echo "release-readiness: --fix-generated cannot be combined with --tag; repair, commit, then verify the tag" >&2
  exit 2
fi

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

capture_candidate_snapshot() {
  python3 - "$repo_root" <<'PY'
import hashlib
import os
import stat
import subprocess
import sys

repo_root = os.fsencode(sys.argv[1])
digest = hashlib.sha256()


def add_field(label: bytes, value: bytes) -> None:
    digest.update(len(label).to_bytes(4, "big"))
    digest.update(label)
    digest.update(len(value).to_bytes(8, "big"))
    digest.update(value)


def git_bytes(*args: str) -> bytes:
    return subprocess.check_output(["git", *args], cwd=repo_root)


# Preserve porcelain state for status-level changes, then add the index and
# filesystem content so an already-dirty path cannot change invisibly.
add_field(b"status", git_bytes("status", "--porcelain=v1", "--untracked-files=all", "-z"))
add_field(b"index", git_bytes("ls-files", "--stage", "-z"))

listed_paths = git_bytes(
    "ls-files", "--cached", "--others", "--exclude-standard", "-z"
).split(b"\0")
for relative_path in sorted(path for path in listed_paths if path):
    add_field(b"path", relative_path)
    absolute_path = os.path.join(repo_root, relative_path)
    try:
        metadata = os.lstat(absolute_path)
    except FileNotFoundError:
        add_field(b"kind", b"missing")
        continue

    add_field(b"mode", str(stat.S_IMODE(metadata.st_mode)).encode("ascii"))
    if stat.S_ISREG(metadata.st_mode):
        content = hashlib.sha256()
        with open(absolute_path, "rb") as candidate_file:
            for chunk in iter(lambda: candidate_file.read(1024 * 1024), b""):
                content.update(chunk)
        add_field(b"kind", b"file")
        add_field(b"content", content.digest())
    elif stat.S_ISLNK(metadata.st_mode):
        add_field(b"kind", b"symlink")
        add_field(b"target", os.readlink(absolute_path))
    else:
        add_field(b"kind", b"other")

print(digest.hexdigest())
PY
}

if [ -n "$release_tag" ] && [ -n "$(git status --porcelain --untracked-files=all)" ]; then
  echo "release-readiness: --tag requires a pristine checkout so the verified source is exactly the tagged commit" >&2
  exit 1
fi

if [ "$fix_generated" -eq 1 ]; then
  echo "release-readiness: regenerating deterministic host surfaces" >&2
  python3 scripts/generate_host_surfaces.py
fi

candidate_head=""
candidate_snapshot=""
if [ "$full" -eq 1 ]; then
  # A shared checkout may change while long Rust or hook suites run. Record
  # the complete candidate state and reject a mixed-tree verdict at the end.
  candidate_head=$(git rev-parse HEAD)
  candidate_snapshot=$(capture_candidate_snapshot)
fi

if [ -n "$release_tag" ]; then
  release_version=${release_tag#v}
  changelog_has_version=0
  if [ -f CHANGELOG.md ] && python3 - "$release_version" <<'PY'
from pathlib import Path
import re
import sys

version = re.escape(sys.argv[1])
heading = re.compile(rf"^##\s+\[?v?{version}\]?(?:\s|$)")
sys.exit(0 if any(heading.match(line) for line in Path("CHANGELOG.md").read_text(encoding="utf-8").splitlines()) else 1)
PY
  then
    changelog_has_version=1
  fi
  if [ "$changelog_has_version" -ne 1 ]; then
    echo "release-readiness: CHANGELOG.md needs a versioned $release_tag heading before release" >&2
    exit 1
  fi
  echo "release-readiness: changelog includes $release_tag" >&2
fi

echo "release-readiness: release parity" >&2
if [ -n "$release_tag" ]; then
  RALLY_RELEASE_TAG="$release_tag" ./scripts/check-release-parity.sh
else
  RALLY_RELEASE_TAG='' ./scripts/check-release-parity.sh
fi

echo "release-readiness: working-tree whitespace" >&2
git diff --check
git diff --cached --check

if [ "$full" -ne 1 ]; then
  echo "release-readiness: candidate checks green" >&2
  exit 0
fi

assert_candidate_unchanged() {
  current_head=$(git rev-parse HEAD)
  current_snapshot=$(capture_candidate_snapshot)
  if [ "$current_head" != "$candidate_head" ]; then
    echo "release-readiness: candidate HEAD changed during --full; rerun from one frozen checkout" >&2
    return 1
  fi
  if [ "$current_snapshot" != "$candidate_snapshot" ]; then
    echo "release-readiness: candidate worktree changed during --full; rerun from one frozen checkout" >&2
    return 1
  fi
}

echo "release-readiness: Rust quality" >&2
if [ -n "${CARGO_TARGET_DIR+x}" ]; then
  ./scripts/run-quality-gate.sh
else
  # A shared checkout can have another agent compiling in target/. Keep this
  # candidate gate independent without changing the caller's tree or target.
  quality_target=$(mktemp -d "${TMPDIR:-/tmp}/rally-release-readiness-target.XXXXXX")
  trap 'rm -rf "$quality_target"' EXIT
  echo "release-readiness: using isolated Cargo target" >&2
  CARGO_TARGET_DIR="$quality_target" ./scripts/run-quality-gate.sh
fi

echo "release-readiness: auxiliary release gates" >&2
if [ -n "${CARGO_TARGET_DIR+x}" ]; then
  ./scripts/run-release-auxiliary-gate.sh
else
  CARGO_TARGET_DIR="$quality_target" ./scripts/run-release-auxiliary-gate.sh
fi

if ! command -v actionlint >/dev/null 2>&1; then
  echo "release-readiness: actionlint is required for --full (install: go install github.com/rhysd/actionlint/cmd/actionlint@<version>)" >&2
  exit 1
fi
echo "release-readiness: workflow syntax" >&2
actionlint

assert_candidate_unchanged
echo "release-readiness: full candidate gate green" >&2

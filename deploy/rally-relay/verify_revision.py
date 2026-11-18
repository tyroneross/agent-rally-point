# SPDX-License-Identifier: Apache-2.0
"""Verify one exact Git commit in a worker-owned checkout of an approved source."""
import json
import re
import subprocess
import sys
import tempfile

revision = json.load(sys.stdin)["text"]
if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", revision):
    sys.exit("expected a full commit SHA")
with tempfile.TemporaryDirectory() as checkout:
    subprocess.run(["git", "clone", "--quiet", "--no-checkout", "--", sys.argv[1], checkout], check=True)
    git = ["git", "-C", checkout]
    commit = subprocess.check_output([*git, "rev-parse", "--verify", revision + "^{commit}"], text=True).strip()
    if commit != revision:
        sys.exit("expected a commit object")
    subprocess.run([*git, "-c", "advice.detachedHead=false", "checkout", "--quiet", "--detach", revision], check=True)
    subprocess.run([*git, "diff-tree", "--root", "--no-commit-id", "--check", "-r", revision], check=True)
    print(commit)

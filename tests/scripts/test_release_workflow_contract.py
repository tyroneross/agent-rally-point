#!/usr/bin/env python3
"""Hermetic contracts for the privileged GitHub Release workflow."""

from __future__ import annotations

import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/release.yml"
GOOD_SHA = "1" * 40
OTHER_SHA = "2" * 40
EXPECTED_ASSETS = [
    "rally-aarch64-apple-darwin",
    "rally-aarch64-apple-darwin.sha256",
    "rally-aarch64-unknown-linux-gnu",
    "rally-aarch64-unknown-linux-gnu.sha256",
    "rally-x86_64-apple-darwin",
    "rally-x86_64-apple-darwin.sha256",
    "rally-x86_64-unknown-linux-gnu",
    "rally-x86_64-unknown-linux-gnu.sha256",
]


def workflow_text() -> str:
    return WORKFLOW.read_text(encoding="utf-8")


def job_block(name: str) -> str:
    text = workflow_text()
    marker = f"  {name}:\n"
    start = text.index(marker)
    end = len(text)
    for candidate in range(start + len(marker), len(text)):
        if text.startswith("  ", candidate) and (
            candidate == 0 or text[candidate - 1] == "\n"
        ):
            line_end = text.find("\n", candidate)
            line = text[candidate:line_end]
            if line.endswith(":") and not line.startswith("    "):
                end = candidate
                break
    return text[start:end]


def step_script(name: str) -> str:
    block = step_block(name)
    lines = block.splitlines()
    run_index = lines.index("        run: |")
    return "\n".join(
        line[10:] if line.startswith("          ") else ""
        for line in lines[run_index + 1 :]
    ) + "\n"


def step_block(name: str) -> str:
    lines = workflow_text().splitlines()
    marker = f"      - name: {name}"
    try:
        step_index = lines.index(marker)
    except ValueError as exc:
        raise AssertionError(f"missing workflow step: {name}") from exc

    body = [lines[step_index]]
    for line in lines[step_index + 1 :]:
        if line.startswith("      - ") or (line and not line.startswith("        ")):
            break
        body.append(line)
    return "\n".join(body) + "\n"


def job_steps(name: str) -> list[str]:
    lines = job_block(name).splitlines()
    starts = [index for index, line in enumerate(lines) if line.startswith("      - ")]
    return [
        "\n".join(lines[start : starts[index + 1] if index + 1 < len(starts) else None])
        for index, start in enumerate(starts)
    ]


def job_permissions(name: str) -> list[str]:
    lines = job_block(name).splitlines()
    start = lines.index("    permissions:") + 1
    permissions: list[str] = []
    for line in lines[start:]:
        if not line.startswith("      "):
            break
        permissions.append(line.strip())
    return permissions


def make_executable(path: Path, content: str) -> None:
    path.write_text(content, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


class ReleaseWorkflowContracts(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.output = self.root / "github-output"
        self.assets = self.root / "assets.tsv"
        self.patch_log = self.root / "patch-called"
        self.gh_log = self.root / "gh-calls"
        self.write_assets(EXPECTED_ASSETS)
        make_executable(
            self.bin / "gh",
            """#!/usr/bin/env bash
set -euo pipefail
printf '%q ' "$@" >> "$TEST_GH_LOG"
printf '\n' >> "$TEST_GH_LOG"

method=GET
endpoint=
jq_filter=
declare -a fields=()
while (($#)); do
  case "$1" in
    --method) method="$2"; shift 2 ;;
    --jq) jq_filter="$2"; shift 2 ;;
    -F|-f) fields+=("$1" "$2"); shift 2 ;;
    repos/*) endpoint="$1"; shift ;;
    *) shift ;;
  esac
done

has_field() {
  local flag="$1" value="$2" index
  for ((index = 0; index < ${#fields[@]}; index += 2)); do
    [[ "${fields[index]}" == "$flag" && "${fields[index + 1]}" == "$value" ]] && return 0
  done
  return 1
}

commit_endpoint="repos/${GITHUB_REPOSITORY}/commits/refs%2Ftags%2F${TAG}"
release_endpoint="repos/${GITHUB_REPOSITORY}/releases/${RELEASE_ID}"
if [[ "$method" == GET && "$endpoint" == "$commit_endpoint" && "$jq_filter" == .sha ]]; then
  case "${TEST_TAG_LOOKUP:-ok}" in
    ok) printf '%s\n' "$TEST_TAG_SHA" ;;
    missing) printf 'gh: No commit found (HTTP 422)\n' >&2; exit 1 ;;
    error) printf 'gh: API unavailable (HTTP 503)\n' >&2; exit 1 ;;
  esac
  exit 0
fi
if [[ "$method" == GET ]] &&
   [[ "$endpoint" == "repos/${GITHUB_REPOSITORY}/releases/tags/${TAG}" ]] &&
   [[ "$jq_filter" == .draft ]]; then
  case "${TEST_RELEASE_LOOKUP:-draft}" in
    draft) printf 'true\n' ;;
    public) printf 'false\n' ;;
    missing) printf 'gh: Not Found (HTTP 404)\n' >&2; exit 1 ;;
    error) printf 'gh: API unavailable (HTTP 503)\n' >&2; exit 1 ;;
  esac
  exit 0
fi
if [[ "$method" == GET ]] &&
   [[ "$endpoint" == "${release_endpoint}/assets?per_page=100" ]] &&
   [[ "$jq_filter" == '.[] | [.name, .state] | @tsv' ]]; then
  [[ "${TEST_ASSET_LOOKUP:-ok}" == ok ]] || { printf 'asset API unavailable\n' >&2; exit 1; }
  cat "$TEST_ASSETS_FILE"
  exit 0
fi
if [[ "$method" == GET && "$endpoint" == "$release_endpoint" ]] &&
   [[ "$jq_filter" == '[.draft, .tag_name] | @tsv' ]]; then
  if [[ "${TEST_RELEASE_DETAIL:-ok}" == malformed ]]; then
    printf 'malformed\n'
  else
    printf '%s\t%s\n' "$TEST_DRAFT" "$TEST_RELEASE_TAG"
  fi
  exit 0
fi
if [[ "$method" == PATCH && "$endpoint" == "$release_endpoint" ]] && \
   has_field -F draft=false && has_field -f make_latest=true; then
  printf 'patched\n' > "$TEST_PATCH_LOG"
  exit "${TEST_PATCH_EXIT:-0}"
fi
if [[ "$method" == PATCH ]]; then
  printf 'invalid PATCH invocation\n' >&2
  exit 96
fi
if [[ -n "$endpoint" ]]; then
  printf 'unexpected gh endpoint: method=%s endpoint=%s jq=%s\n' "$method" "$endpoint" "$jq_filter" >&2
  exit 97
fi
printf 'unexpected gh invocation\n' >&2
exit 97
""",
        )

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def write_assets(
        self,
        names: list[str],
        *,
        state: str = "uploaded",
        states: dict[str, str] | None = None,
    ) -> None:
        state_by_name = states or {}
        self.assets.write_text(
            "".join(f"{name}\t{state_by_name.get(name, state)}\n" for name in names),
            encoding="utf-8",
        )

    def run_script(
        self, script: str, *, extra_env: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        self.output.write_text("", encoding="utf-8")
        self.patch_log.unlink(missing_ok=True)
        self.gh_log.write_text("", encoding="utf-8")
        env = {
            "PATH": f"{self.bin}:/usr/bin:/bin",
            "GH_TOKEN": "test-token",
            "GITHUB_REPOSITORY": "tyroneross/agent-rally-point",
            "GITHUB_OUTPUT": str(self.output),
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_REF": "refs/heads/main",
            "GITHUB_SHA": GOOD_SHA,
            "DEFAULT_BRANCH": "main",
            "INPUT_TAG": "v0.2.5",
            "TAG": "v0.2.5",
            "EXPECTED_SHA": GOOD_SHA,
            "RELEASE_ID": "12345",
            "TEST_TAG_SHA": GOOD_SHA,
            "TEST_TAG_LOOKUP": "ok",
            "TEST_RELEASE_LOOKUP": "draft",
            "TEST_ASSETS_FILE": str(self.assets),
            "TEST_DRAFT": "true",
            "TEST_RELEASE_TAG": "v0.2.5",
            "TEST_PATCH_LOG": str(self.patch_log),
            "TEST_GH_LOG": str(self.gh_log),
            "TEST_ASSET_LOOKUP": "ok",
            "TEST_RELEASE_DETAIL": "ok",
        }
        if extra_env:
            env.update(extra_env)
        return subprocess.run(
            ["bash", "-c", script],
            cwd=self.root,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_resolver_accepts_only_default_branch_dispatch_and_strict_tag(self) -> None:
        script = step_script("Validate trigger and resolve tag commit")
        self.assertIn('/commits/refs%2Ftags%2F${tag}', script)
        resolver_step = step_block("Validate trigger and resolve tag commit")
        for wiring in (
            "id: release",
            "GH_TOKEN: ${{ github.token }}",
            "DEFAULT_BRANCH: ${{ github.event.repository.default_branch }}",
            "INPUT_TAG: ${{ github.event.inputs.tag }}",
        ):
            self.assertIn(wiring, resolver_step)
        result = self.run_script(script)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.output.read_text(encoding="utf-8"),
            f"tag=v0.2.5\nsha={GOOD_SHA}\n",
        )

        wrong_ref = self.run_script(
            script, extra_env={"GITHUB_REF": "refs/heads/release-experiment"}
        )
        self.assertNotEqual(wrong_ref.returncode, 0)
        self.assertIn("must be dispatched", wrong_ref.stderr)

        for invalid_tag in (
            "v0.2",
            "v0.2.5-rc1",
            "v0.2.5+build",
            "v01.2.3",
            "v0.02.3",
            "v0.2.03",
            "v0.2.5\nforged=true",
        ):
            with self.subTest(invalid_tag=invalid_tag):
                result = self.run_script(script, extra_env={"INPUT_TAG": invalid_tag})
                self.assertNotEqual(result.returncode, 0)

        missing = self.run_script(
            script, extra_env={"TEST_TAG_LOOKUP": "missing"}
        )
        self.assertNotEqual(missing.returncode, 0)

        for invalid_sha in ("null", "abc123", "G" * 40):
            with self.subTest(invalid_sha=invalid_sha):
                result = self.run_script(
                    script, extra_env={"TEST_TAG_SHA": invalid_sha}
                )
                self.assertNotEqual(result.returncode, 0)

    def test_resolver_binds_tag_push_to_event_sha(self) -> None:
        script = step_script("Validate trigger and resolve tag commit")
        valid = self.run_script(
            script,
            extra_env={
                "GITHUB_EVENT_NAME": "push",
                "GITHUB_REF": "refs/tags/v0.2.5",
                "GITHUB_SHA": GOOD_SHA,
                "INPUT_TAG": "v9.9.9",
            },
        )
        self.assertEqual(valid.returncode, 0, valid.stderr)

        branch_push = self.run_script(
            script,
            extra_env={
                "GITHUB_EVENT_NAME": "push",
                "GITHUB_REF": "refs/heads/main",
            },
        )
        self.assertNotEqual(branch_push.returncode, 0)

        unknown = self.run_script(script, extra_env={"GITHUB_EVENT_NAME": "schedule"})
        self.assertNotEqual(unknown.returncode, 0)

        moved = self.run_script(
            script,
            extra_env={
                "GITHUB_EVENT_NAME": "push",
                "GITHUB_REF": "refs/tags/v0.2.5",
                "GITHUB_SHA": OTHER_SHA,
            },
        )
        self.assertNotEqual(moved.returncode, 0)
        self.assertIn("tag moved", moved.stderr)

    def test_all_source_jobs_use_the_resolved_sha_and_least_privilege(self) -> None:
        text = workflow_text()
        self.assertIn("permissions: {}", text)
        self.assertNotIn(
            "ref: ${{ github.event.inputs.tag || github.ref }}", text
        )
        self.assertNotIn(
            "tag_name: ${{ github.event.inputs.tag || github.ref_name }}", text
        )

        resolve = job_block("resolve")
        self.assertEqual(job_permissions("resolve"), ["contents: read"])
        self.assertIn("sha: ${{ steps.release.outputs.sha }}", resolve)
        self.assertNotIn("actions/checkout", resolve)

        for name in ("quality", "parity", "build"):
            with self.subTest(job=name):
                block = job_block(name)
                header = block.split("    steps:", 1)[0]
                expected_needs = (
                    "    needs: resolve\n"
                    if name in ("quality", "parity")
                    else "    needs: [resolve, quality, parity]\n"
                )
                self.assertIn(expected_needs, header)
                checkouts = [
                    step
                    for step in job_steps(name)
                    if "uses: actions/checkout@" in step
                ]
                self.assertEqual(len(checkouts), 1)
                self.assertIn("ref: ${{ needs.resolve.outputs.sha }}", checkouts[0])
                self.assertIn("persist-credentials: false", checkouts[0])

                if name in ("quality", "parity"):
                    self.assertEqual(job_permissions(name), ["contents: read"])
                    self.assertNotIn("id-token: write", header)
                    self.assertNotIn("attestations: write", header)
                else:
                    self.assertEqual(
                        job_permissions(name),
                        [
                            "contents: read",
                            "id-token: write",
                            "attestations: write",
                        ],
                    )

        self.assertIn("fetch-depth: 0", job_block("parity"))
        self.assertIn(
            "RALLY_RELEASE_TAG: ${{ needs.resolve.outputs.tag }}",
            job_block("parity"),
        )
        publish = job_block("publish")
        self.assertIn("needs: [resolve, quality, parity, build]", publish)
        self.assertEqual(
            job_permissions("publish"), ["actions: read", "contents: write"]
        )
        self.assertNotIn("actions/checkout", publish)
        self.assertNotIn("id-token: write", publish)
        self.assertNotIn("attestations: write", publish)

        contract_command = "python3 tests/scripts/test_release_workflow_contract.py"
        release_quality = job_block("quality")
        for strict_release_control in (
            "cargo install cargo-audit cargo-deny --locked",
            "python3 tests/scripts/test_release_readiness.py",
            "python3 tests/scripts/test_run_release_auxiliary_gate.py",
            contract_command,
            "run: ./scripts/run-release-auxiliary-gate.sh\n",
        ):
            self.assertIn(strict_release_control, release_quality)
        self.assertNotIn("cargo install cargo-nextest", release_quality)
        release_gate = step_block("Quality gate (scripts/run-quality-gate.sh)")
        self.assertIn("RALLY_QG_TEST_MODE: serial", release_gate)
        self.assertIn("run: ./scripts/run-quality-gate.sh", release_gate)
        self.assertNotIn("run-release-auxiliary-gate.sh --product-only", release_quality)
        ci = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn(contract_command, ci)
        self.assertIn("./scripts/run-release-auxiliary-gate.sh --product-only", ci)
        self.assertNotIn("cargo install cargo-nextest", ci)
        self.assertNotIn("cargo install cargo-audit", ci)
        self.assertNotIn("test_release_readiness.py", ci)

        stage = step_block("Stage all release assets in a draft")
        for setting in (
            "id: upload_draft",
            "uses: softprops/action-gh-release@v3",
            "tag_name: ${{ needs.resolve.outputs.tag }}",
            "target_commitish: ${{ needs.resolve.outputs.sha }}",
            "draft: true",
            "generate_release_notes: true",
            "fail_on_unmatched_files: true",
            "files: release-assets/*",
        ):
            self.assertIn(setting, stage)

        publish_steps = job_steps("publish")
        self.assertEqual(
            [step.splitlines()[0] for step in publish_steps],
            [
                "      - name: Download complete build matrix",
                "      - name: Verify every target produced its binary and SHA256 sidecar",
                "      - name: Require immutable tag and a fresh or draft release",
                "      - name: Stage all release assets in a draft",
                "      - name: Verify draft assets and publish as latest",
            ],
        )
        for forbidden in (
            "./release-assets",
            "bash release-assets",
            "sh release-assets",
            "chmod ",
            "source ",
        ):
            self.assertNotIn(forbidden, publish)

    def test_release_serial_mode_ignores_an_ambient_nextest(self) -> None:
        cargo_log = self.root / "cargo.log"
        make_executable(
            self.bin / "git",
            f"""#!/bin/sh
if [ "$1 $2" = "rev-parse --show-toplevel" ]; then
  printf '%s\\n' {str(ROOT)!r}
  exit 0
fi
exit 1
""",
        )
        make_executable(
            self.bin / "cargo",
            f"""#!/bin/sh
printf '%s\\n' "$*" >> {str(cargo_log)!r}
""",
        )
        make_executable(self.bin / "cargo-nextest", "#!/bin/sh\nexit 99\n")

        env = os.environ.copy()
        env.update(
            {
                "PATH": str(self.bin),
                "RALLY_QG_TEST_MODE": "serial",
                "RALLY_QG_TOOLCHAIN": "fixture-toolchain-not-installed",
            }
        )
        result = subprocess.run(
            ["/bin/bash", str(ROOT / "scripts/run-quality-gate.sh")],
            cwd=ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        cargo_calls = cargo_log.read_text(encoding="utf-8").splitlines()
        self.assertIn("test --workspace -- --test-threads=1", cargo_calls)
        self.assertFalse(any("nextest" in call for call in cargo_calls))

    def test_pre_upload_guard_rejects_a_moved_tag_or_unknown_release_state(self) -> None:
        script = step_script("Require immutable tag and a fresh or draft release")
        guard = step_block("Require immutable tag and a fresh or draft release")
        for wiring in (
            "GH_TOKEN: ${{ github.token }}",
            "TAG: ${{ needs.resolve.outputs.tag }}",
            "EXPECTED_SHA: ${{ needs.resolve.outputs.sha }}",
        ):
            self.assertIn(wiring, guard)

        draft = self.run_script(script)
        self.assertEqual(draft.returncode, 0, draft.stderr)
        fresh = self.run_script(script, extra_env={"TEST_RELEASE_LOOKUP": "missing"})
        self.assertEqual(fresh.returncode, 0, fresh.stderr)

        moved = self.run_script(script, extra_env={"TEST_TAG_SHA": OTHER_SHA})
        self.assertNotEqual(moved.returncode, 0)
        self.assertIn("tag moved", moved.stderr)

        public = self.run_script(script, extra_env={"TEST_RELEASE_LOOKUP": "public"})
        self.assertNotEqual(public.returncode, 0)

        unavailable = self.run_script(
            script, extra_env={"TEST_RELEASE_LOOKUP": "error"}
        )
        self.assertNotEqual(unavailable.returncode, 0)
        self.assertIn("refusing publication", unavailable.stderr)

    def test_exact_uploaded_draft_assets_are_required_before_patch(self) -> None:
        script = step_script("Verify draft assets and publish as latest")
        good = self.run_script(script)
        self.assertEqual(good.returncode, 0, good.stderr)
        self.assertTrue(self.patch_log.exists())

        variants = {
            "missing": EXPECTED_ASSETS[:-1],
            "extra": [*EXPECTED_ASSETS, "rally-unexpected"],
            "duplicate": [*EXPECTED_ASSETS, EXPECTED_ASSETS[0]],
        }
        for label, assets in variants.items():
            with self.subTest(label=label):
                self.write_assets(assets)
                result = self.run_script(script)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.patch_log.exists())

        self.write_assets(
            EXPECTED_ASSETS,
            states={EXPECTED_ASSETS[0]: "starter"},
        )
        starter = self.run_script(script)
        self.assertNotEqual(starter.returncode, 0)
        self.assertFalse(self.patch_log.exists())

        asset_error = self.run_script(
            script, extra_env={"TEST_ASSET_LOOKUP": "error"}
        )
        self.assertNotEqual(asset_error.returncode, 0)
        self.assertFalse(self.patch_log.exists())

    def test_publish_rechecks_draft_tag_and_sha_in_the_patch_step(self) -> None:
        script = step_script("Verify draft assets and publish as latest")
        final_step = step_block("Verify draft assets and publish as latest")
        for wiring in (
            "GH_TOKEN: ${{ github.token }}",
            "TAG: ${{ needs.resolve.outputs.tag }}",
            "EXPECTED_SHA: ${{ needs.resolve.outputs.sha }}",
            "RELEASE_ID: ${{ steps.upload_draft.outputs.id }}",
        ):
            self.assertIn(wiring, final_step)
        cases = (
            ("public", {"TEST_DRAFT": "false"}),
            ("wrong-tag", {"TEST_RELEASE_TAG": "v9.9.9"}),
            ("moved-tag", {"TEST_TAG_SHA": OTHER_SHA}),
            ("invalid-id", {"RELEASE_ID": "not-a-number"}),
            ("empty-id", {"RELEASE_ID": ""}),
            ("zero-id", {"RELEASE_ID": "0"}),
            ("malformed-release", {"TEST_RELEASE_DETAIL": "malformed"}),
        )
        for label, env in cases:
            with self.subTest(label=label):
                self.write_assets(EXPECTED_ASSETS)
                result = self.run_script(script, extra_env=env)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.patch_log.exists())

        compare = script.index('if [ "$current_sha" != "$EXPECTED_SHA" ]')
        patch = script.index("gh api --method PATCH")
        self.assertLess(compare, patch)
        self.assertNotIn("actions/checkout", job_block("publish"))

        self.write_assets(EXPECTED_ASSETS)
        good = self.run_script(script)
        self.assertEqual(good.returncode, 0, good.stderr)
        calls = self.gh_log.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(calls), 4)
        self.assertIn("/releases/12345", calls[0])
        self.assertIn("/releases/12345/assets\\?per_page=100", calls[1])
        self.assertIn("/commits/refs%2Ftags%2Fv0.2.5", calls[2])
        self.assertIn("--method PATCH", calls[3])
        self.assertIn("/releases/12345", calls[3])
        self.assertIn("draft=false", calls[3])
        self.assertIn("make_latest=true", calls[3])


if __name__ == "__main__":
    unittest.main()

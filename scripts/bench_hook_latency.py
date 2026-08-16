#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""bench_hook_latency.py -- before-write hook latency harness (build-loop plan C0).

Times `bash hooks/rally-coordination-hook.sh before-write <tool>` end-to-end
from Python (time.perf_counter around subprocess.run) for three scenarios:

  claude_1path       -- Claude Code Edit, one absolute target path
  codex_4path        -- Codex apply_patch, four cwd-relative `*** Update File:`
                         directives (the O33-A classifier requires apply_patch
                         targets to be cwd-relative; an absolute path there is
                         classified malformed)
  claude_pure_read   -- Claude Code Read; should short-circuit before any
                         Rally root walk or ledger work

Reports p50/p95/min/max/mean per scenario, os.getloadavg() at harness start
and end (and again per scenario), the `rally` build id, the hook file's
sha256, an as-of timestamp, and a best-effort subprocess-spawn attribution
(node/perl/rally invocation counts) captured via one traced `bash -x` fire per
scenario.

Absolute latency thresholds referenced elsewhere in this repo's docs/plans are
reference points, not pass conditions -- see CLAUDE.md "Performance claims"
and .build-loop/goal.md criteria 12-14. This script reports what it measures,
not a verdict, and refuses to report a number for a scenario it cannot prove
actually exercised the code path it claims to (see verify_scenario_effect()).

Python 3 stdlib only -- no new dependencies.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent
HOOK_PATH = REPO_ROOT / "hooks" / "rally-coordination-hook.sh"
SCENARIOS = ("claude_1path", "codex_4path", "claude_pure_read")


def eprint(*args, **kwargs):
    print(*args, file=sys.stderr, **kwargs)
    sys.stderr.flush()


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def percentile(values, pct):
    if not values:
        return None
    s = sorted(values)
    if len(s) == 1:
        return s[0]
    k = (len(s) - 1) * (pct / 100.0)
    lo = int(k)
    hi = min(lo + 1, len(s) - 1)
    if lo == hi:
        return s[lo]
    return s[lo] * (hi - k) + s[hi] * (k - lo)


def default_rally_bin() -> Path:
    """Prefer a release binary already built in this worktree; fall back to
    debug, then PATH. Reused, never rebuilt -- the caller records its build id."""
    for cand in (REPO_ROOT / "target" / "release" / "rally", REPO_ROOT / "target" / "debug" / "rally"):
        if cand.is_file():
            return cand
    found = shutil.which("rally")
    if found:
        return Path(found)
    raise SystemExit(
        "no rally binary found (checked target/release/rally, target/debug/rally, PATH). "
        "Build one first: cargo build --release -p rally-cli"
    )


def rally_build_id(rally_bin: Path) -> str:
    result = subprocess.run([str(rally_bin), "version", "--json"], capture_output=True, text=True, timeout=10)
    try:
        data = json.loads(result.stdout)
        return data["data"]["version"]["build_id"]
    except Exception as exc:  # noqa: BLE001 -- reporting the raw failure is the point
        return f"unknown (rally version --json parse failed: {exc}; stdout={result.stdout!r} stderr={result.stderr!r})"


# --- scratch repo -------------------------------------------------------------

def _run_checked(cmd, cwd=None, env=None, input_bytes=None, timeout=30):
    result = subprocess.run(cmd, cwd=cwd, env=env, input=input_bytes, capture_output=True, timeout=timeout)
    if result.returncode != 0:
        raise RuntimeError(
            f"command failed ({result.returncode}): {cmd}\n"
            f"stdout={result.stdout.decode(errors='replace')}\nstderr={result.stderr.decode(errors='replace')}"
        )
    return result


def setup_scratch_repo(rally_bin: Path) -> Path:
    """Fresh scratch repo under /var/tmp: git init, a seed commit, a real
    `.rally` store via `rally init`, and one peer claim on src/peer_only.rs so
    every scenario has a genuine unowned/claimed contrast to check against."""
    root = Path(tempfile.mkdtemp(prefix="rally_bench_", dir="/var/tmp")).resolve()
    _run_checked(["git", "init", "-q"], cwd=str(root))
    _run_checked(["git", "config", "user.email", "bench@example.invalid"], cwd=str(root))
    _run_checked(["git", "config", "user.name", "bench"], cwd=str(root))
    (root / "README.md").write_text("rally bench_hook_latency scratch repo\n")
    _run_checked(["git", "add", "README.md"], cwd=str(root))
    _run_checked(["git", "commit", "-q", "-m", "seed"], cwd=str(root))
    _run_checked([str(rally_bin), "init", "--json"], cwd=str(root))
    _run_checked([str(rally_bin), "enter", "--tool", "bench:peer", "--json"], cwd=str(root))
    _run_checked(
        [str(rally_bin), "say", "claim", "--tool", "bench:peer", "--path", "src/peer_only.rs", "--subject", "peer", "--json"],
        cwd=str(root),
    )
    (root / "src").mkdir(exist_ok=True)
    (root / "patchdir").mkdir(exist_ok=True)
    return root


def ledger_lines(root: Path) -> int:
    log_dir = root / ".rally" / "log"
    if not log_dir.is_dir():
        return 0
    total = 0
    for p in log_dir.rglob("*.jsonl"):
        with open(p, "rb") as f:
            total += sum(1 for _ in f)
    return total


# --- envelopes ------------------------------------------------------------

def envelope_claude_1path(scratch: Path, idx: int):
    """Claude Edit, one absolute target path, varied per iteration so the
    working-status dedupe marker and claim-idempotency check don't turn
    iterations 2..n into a cheaper cached path."""
    target = scratch / "src" / f"bench_{idx}.rs"
    env = {"tool_name": "Edit", "tool_input": {"file_path": str(target)}}
    return "claude_code", json.dumps(env).encode(), scratch


def envelope_codex_4path(scratch: Path, idx: int):
    """Codex apply_patch with FOUR `*** Update File:` directives. Targets are
    cwd-relative basenames (the O33-A classifier rejects an absolute apply_patch
    target as malformed) and the subprocess cwd is set to patchdir/ so they
    resolve. This must NOT be expressed as a Bash tool call -- Bash is in the
    OPAQUE_SHELL_TOOLS table and short-circuits before any classification."""
    names = [f"p{n}_{idx}.rs" for n in range(4)]
    body = "\n".join(["*** Begin Patch"] + [f"*** Update File: {name}" for name in names] + ["*** End Patch"])
    env = {"tool_name": "apply_patch", "tool_input": {"command": body}}
    return "codex", json.dumps(env).encode(), scratch / "patchdir"


def envelope_claude_pure_read(scratch: Path, idx: int):
    target = scratch / "README.md"
    env = {"tool_name": "Read", "tool_input": {"file_path": str(target)}}
    return "claude_code", json.dumps(env).encode(), scratch


ENVELOPE_BUILDERS = {
    "claude_1path": envelope_claude_1path,
    "codex_4path": envelope_codex_4path,
    "claude_pure_read": envelope_claude_pure_read,
}


# --- firing the hook --------------------------------------------------------

def _hook_env(rally_bin: Path, host_tool: str, session_id: str, extra=None):
    env = dict(os.environ)
    env["RALLY_BIN"] = str(rally_bin)
    env["RALLY_SESSION_ID"] = session_id
    env["RALLY_TOOL_ID"] = f"{host_tool}:{session_id}"
    if extra:
        env.update(extra)
    return env


def fire_hook(rally_bin: Path, host_tool: str, envelope_bytes: bytes, cwd: Path, session_id: str, timeout=30):
    env = _hook_env(rally_bin, host_tool, session_id)
    start = time.perf_counter()
    result = subprocess.run(
        ["bash", str(HOOK_PATH), "before-write", host_tool],
        input=envelope_bytes,
        cwd=str(cwd),
        env=env,
        capture_output=True,
        timeout=timeout,
    )
    elapsed_ms = (time.perf_counter() - start) * 1000.0
    return elapsed_ms, result


def verify_scenario_effect(name: str, stdout_text: str, ledger_before: int, ledger_after: int):
    """Requirement: a scenario that silently no-ops must FAIL the harness
    loudly, not report a fast number. pure_read must short-circuit (stdout
    "{}", zero ledger delta); the mutation scenarios must prove they exercised
    the transaction (a ledger append) or produced a visible non-"{}" signal."""
    delta = ledger_after - ledger_before
    if name == "claude_pure_read":
        if stdout_text != "{}" or delta != 0:
            raise RuntimeError(
                f"{name}: validity check FAILED -- expected stdout '{{}}' and zero ledger delta "
                f"(short-circuit before any Rally root walk), got stdout={stdout_text!r} ledger_delta={delta}. "
                "This scenario is not exercising the pure-read short-circuit; refusing to report a number."
            )
    else:
        if delta == 0 and stdout_text == "{}":
            raise RuntimeError(
                f"{name}: validity check FAILED -- mutation produced neither a ledger append nor a "
                f"non-'{{}}' envelope (ledger_before={ledger_before} ledger_after={ledger_after} "
                f"stdout={stdout_text!r}). The scenario silently no-opped; refusing to report a number."
            )


# --- spawn attribution -------------------------------------------------------

def _run_with_fd9(argv, cwd, env, input_bytes, trace_fd, timeout=30):
    def preexec():
        os.dup2(trace_fd, 9)

    return subprocess.run(
        argv, cwd=cwd, env=env, input=input_bytes,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        preexec_fn=preexec, timeout=timeout,
    )


def probe_xtracefd_support() -> bool:
    """Empirically check whether this host's bash honors BASH_XTRACEFD (added
    in bash 4.1; macOS system bash is 3.2 and does not). Never trust a version
    string alone -- run the real mechanism and check the file got bytes."""
    trace_path = Path(tempfile.mkstemp(dir="/var/tmp")[1])
    try:
        fd = os.open(str(trace_path), os.O_WRONLY | os.O_TRUNC)
        try:
            env = dict(os.environ)
            env["BASH_XTRACEFD"] = "9"
            _run_with_fd9(["bash", "-x", "-c", "echo probe"], None, env, None, fd, timeout=5)
        finally:
            os.close(fd)
        return trace_path.stat().st_size > 0
    except Exception:  # noqa: BLE001 -- any failure means "not supported here"
        return False
    finally:
        trace_path.unlink(missing_ok=True)


def traced_fire(rally_bin: Path, host_tool: str, envelope_bytes: bytes, cwd: Path, session_id: str,
                 use_fd9: bool, unit_costs: dict):
    """One traced fire. Prefers BASH_XTRACEFD (fd9) when the host bash
    supports it; otherwise falls back to bash -x's default stderr target,
    which is separable here because a happy-path fire prints nothing else to
    stderr. If tracing itself errors out, spawns is recorded as null with a
    reason -- per the harness contract, that must not fail the whole run."""
    try:
        if use_fd9:
            trace_path = Path(tempfile.mkstemp(dir="/var/tmp")[1])
            fd = os.open(str(trace_path), os.O_WRONLY | os.O_TRUNC)
            try:
                env = _hook_env(rally_bin, host_tool, session_id, extra={"BASH_XTRACEFD": "9"})
                _run_with_fd9(["bash", "-x", str(HOOK_PATH), "before-write", host_tool], str(cwd), env, envelope_bytes, fd)
            finally:
                os.close(fd)
            trace_text = trace_path.read_text(errors="replace")
            trace_path.unlink(missing_ok=True)
            method = "fd9"
            reason = None
        else:
            env = _hook_env(rally_bin, host_tool, session_id)
            result = subprocess.run(
                ["bash", "-x", str(HOOK_PATH), "before-write", host_tool],
                input=envelope_bytes, cwd=str(cwd), env=env, capture_output=True, timeout=30,
            )
            trace_text = result.stderr.decode(errors="replace")
            method = "stderr_fallback"
            reason = (
                "system bash does not honor BASH_XTRACEFD from the environment (probe failed; "
                "this feature requires bash>=4.1, macOS system /bin/bash is 3.2) -- "
                "xtrace was captured from its default stderr target instead"
            )

        node_e = len(re.findall(r"(?m)^\+.*node -e", trace_text))
        perl_dash = len(re.findall(r"(?m)^\+.*perl -", trace_text))
        rally_bin_hits = trace_text.count(str(rally_bin))
        plus_lines = len(re.findall(r"(?m)^\+", trace_text))
        doubleplus_lines = len(re.findall(r"(?m)^\+\+", trace_text))

        overhead_ms = 0.0
        if unit_costs.get("node_e_empty_ms") is not None:
            overhead_ms += node_e * unit_costs["node_e_empty_ms"]
        if unit_costs.get("perl_e_1_ms") is not None:
            overhead_ms += perl_dash * unit_costs["perl_e_1_ms"]

        return {
            "method": method,
            "reason": reason,
            "node_e_count": node_e,
            "perl_dash_count": perl_dash,
            "rally_bin_literal_occurrences": rally_bin_hits,
            "plus_prefixed_lines_total": plus_lines,
            "subshell_approx_doubleplus_lines": doubleplus_lines,
            "estimated_spawn_overhead_ms": round(overhead_ms, 3),
            "trace_bytes": len(trace_text),
        }
    except Exception as exc:  # noqa: BLE001 -- tracing failure must not fail the run
        return {"method": None, "reason": f"bash -x tracing failed: {exc}"}


def unit_startup_costs(rally_bin: Path, reps: int = 5) -> dict:
    """Measured once (not per scenario): node -e "", perl -e 1, bash -c :,
    and one `rally version --json` (capabilities doesn't exist on this
    pre-C1 baseline, so version is the closest equivalent single-call cost)."""

    def median_ms(argv):
        samples = []
        for _ in range(reps):
            start = time.perf_counter()
            subprocess.run(argv, capture_output=True, timeout=10)
            samples.append((time.perf_counter() - start) * 1000.0)
        return round(statistics.median(samples), 3)

    costs = {}
    node = shutil.which("node")
    costs["node_e_empty_ms"] = median_ms([node, "-e", ""]) if node else None
    perl = shutil.which("perl")
    costs["perl_e_1_ms"] = median_ms([perl, "-e", "1"]) if perl else None
    costs["bash_c_colon_ms"] = median_ms(["bash", "-c", ":"])
    costs["rally_version_json_ms"] = median_ms([str(rally_bin), "version", "--json"])
    costs["reps"] = reps
    return costs


# --- load burners ------------------------------------------------------------

def spawn_burners(n: int):
    procs = []
    for _ in range(n):
        procs.append(subprocess.Popen(["bash", "-c", "yes >/dev/null"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
    return procs


def kill_burners(procs):
    # Kill exactly the PIDs we recorded when we spawned them. NEVER pkill -f --
    # that matches machine-wide and would kill unrelated processes.
    for p in procs:
        try:
            p.kill()
        except Exception:  # noqa: BLE001
            pass
    for p in procs:
        try:
            p.wait(timeout=5)
        except Exception:  # noqa: BLE001
            pass


# --- scenario runner ---------------------------------------------------------

def run_scenario(name: str, rally_bin: Path, scratch: Path, n: int, use_fd9: bool, unit_costs: dict):
    builder = ENVELOPE_BUILDERS[name]
    session_id = f"bench-{name}"
    loadavg_start = os.getloadavg()

    counter = 0

    def next_idx():
        nonlocal counter
        counter += 1
        return counter

    # Warm-up + validity check (untimed, not counted in stats).
    idx = next_idx()
    host_tool, envelope_bytes, cwd = builder(scratch, idx)
    ledger_before = ledger_lines(scratch)
    warm_elapsed_ms, warm_result = fire_hook(rally_bin, host_tool, envelope_bytes, cwd, session_id)
    if warm_result.returncode != 0:
        raise RuntimeError(f"{name}: warm-up call exited {warm_result.returncode}, stderr={warm_result.stderr!r}")
    ledger_after = ledger_lines(scratch)
    stdout_text = warm_result.stdout.decode(errors="replace").strip()
    verify_scenario_effect(name, stdout_text, ledger_before, ledger_after)

    # One traced fire (spawn attribution), also untimed/uncounted.
    idx = next_idx()
    host_tool, envelope_bytes, cwd = builder(scratch, idx)
    spawns = traced_fire(rally_bin, host_tool, envelope_bytes, cwd, session_id, use_fd9, unit_costs)

    # Timed loop.
    samples_ms = []
    for _ in range(n):
        idx = next_idx()
        host_tool, envelope_bytes, cwd = builder(scratch, idx)
        elapsed_ms, result = fire_hook(rally_bin, host_tool, envelope_bytes, cwd, session_id)
        if result.returncode != 0:
            raise RuntimeError(f"{name}: iteration {idx} exited {result.returncode}, stderr={result.stderr!r}")
        samples_ms.append(elapsed_ms)

    loadavg_end = os.getloadavg()

    return {
        "n": len(samples_ms),
        "p50_ms": round(percentile(samples_ms, 50), 3),
        "p95_ms": round(percentile(samples_ms, 95), 3),
        "min_ms": round(min(samples_ms), 3),
        "max_ms": round(max(samples_ms), 3),
        "mean_ms": round(statistics.mean(samples_ms), 3),
        "stdev_ms": round(statistics.pstdev(samples_ms), 3) if len(samples_ms) > 1 else 0.0,
        "loadavg": {"at_start": list(loadavg_start), "at_end": list(loadavg_end)},
        "spawns": spawns,
        "warm_up": {
            "elapsed_ms": round(warm_elapsed_ms, 3),
            "stdout": stdout_text,
            "ledger_delta": ledger_after - ledger_before,
        },
        "raw_samples_ms": [round(v, 3) for v in samples_ms],
    }


# --- main --------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repeat", type=int, default=20, help="iterations per scenario, n>=10 (default 20)")
    parser.add_argument("--out", type=Path, default=None, help="write the JSON artifact to this path")
    parser.add_argument("--load", type=int, default=0, help="spawn N `yes` burner processes for the run's duration")
    parser.add_argument("--keep", action="store_true", help="keep the scratch repo instead of deleting it on exit")
    parser.add_argument("--rally-bin", type=Path, default=None, help="override the rally binary under test")
    args = parser.parse_args()

    if args.repeat < 10:
        eprint(f"warning: --repeat {args.repeat} is below the plan's n>=10 floor; results will be noisier")

    if not HOOK_PATH.is_file():
        raise SystemExit(f"hook not found at {HOOK_PATH}")

    rally_bin = (args.rally_bin or default_rally_bin()).resolve()
    if not rally_bin.is_file():
        raise SystemExit(f"rally binary not found at {rally_bin}")

    as_of = datetime.now(timezone.utc).isoformat()
    build_id = rally_build_id(rally_bin)
    hook_sha256 = sha256_file(HOOK_PATH)

    print(f"as-of: {as_of}")
    print(f"rally build_id: {build_id}")
    print(f"rally binary: {rally_bin}")
    print(f"hook: {HOOK_PATH} (sha256 {hook_sha256})")

    loadavg_harness_start = os.getloadavg()
    print(f"loadavg at harness start (1m,5m,15m): {loadavg_harness_start}")
    if loadavg_harness_start[0] >= 2:
        print(f"NOTE: load-avg 1m={loadavg_harness_start[0]:.2f} -- this run is NOT unloaded; do not label it so")

    use_fd9 = probe_xtracefd_support()
    print(f"xtrace attribution method: {'fd9 (BASH_XTRACEFD)' if use_fd9 else 'stderr_fallback (BASH_XTRACEFD unsupported on this bash)'}")

    unit_costs = unit_startup_costs(rally_bin)
    print(f"unit startup costs (median of {unit_costs['reps']}, ms): {unit_costs}")

    burners = []
    if args.load > 0:
        print(f"spawning {args.load} `yes` burner processes for the run's duration")
        burners = spawn_burners(args.load)

    scratch = setup_scratch_repo(rally_bin)
    print(f"scratch repo: {scratch}")

    # Warm the hook file itself once, untimed, before any scenario times
    # anything (tests/hooks/test_rally_coordination_hook.sh:44-103 install_stub
    # rationale: pay first-exec OS evaluation cost outside the timed loop).
    # hooks/rally-coordination-hook.sh is a long-tracked repo file, not a
    # freshly minted stub, so this mostly just warms page cache / node startup.
    try:
        warm_env = _hook_env(rally_bin, "claude_code", "harness-warmup")
        warm_input = json.dumps({"tool_name": "Read", "tool_input": {"file_path": str(scratch / "README.md")}}).encode()
        subprocess.run(["bash", str(HOOK_PATH), "before-write", "claude_code"], input=warm_input,
                        cwd=str(scratch), env=warm_env, capture_output=True, timeout=10)
    except Exception as exc:  # noqa: BLE001 -- non-fatal; continue to timed scenarios
        eprint(f"harness warm-up call failed (non-fatal, continuing): {exc}")

    results = {}
    exit_code = 0
    failed_scenario = None
    name = None
    try:
        for name in SCENARIOS:
            print(f"--- {name} ---")
            r = run_scenario(name, rally_bin, scratch, args.repeat, use_fd9, unit_costs)
            r["build_id"] = build_id
            results[name] = r
            print(
                f"{name}: p50={r['p50_ms']}ms p95={r['p95_ms']}ms min={r['min_ms']}ms max={r['max_ms']}ms "
                f"n={r['n']} loadavg_start={r['loadavg']['at_start']} loadavg_end={r['loadavg']['at_end']} "
                f"spawns.method={r['spawns'].get('method')}"
            )
    except Exception as exc:  # noqa: BLE001 -- fail loudly, do not report a fast number
        failed_scenario = name or "?"
        eprint(f"FATAL: scenario failed: {exc}")
        exit_code = 1
    finally:
        kill_burners(burners)
        if not args.keep:
            shutil.rmtree(scratch, ignore_errors=True)
        else:
            print(f"scratch repo kept at {scratch} (--keep)")

    if exit_code != 0:
        eprint(f"bench_hook_latency.py: exiting non-zero, scenario '{failed_scenario}' did not validate. No artifact written.")
        return exit_code

    loadavg_harness_end = os.getloadavg()
    artifact = {
        "as_of": as_of,
        "build_id": build_id,
        "rally_bin": str(rally_bin),
        "hook_path": str(HOOK_PATH),
        "hook_sha256": hook_sha256,
        "repeat": args.repeat,
        "load_burners": args.load,
        "loadavg_harness_start": list(loadavg_harness_start),
        "loadavg_harness_end": list(loadavg_harness_end),
        "unit_startup_costs_ms": unit_costs,
        "xtrace_method": "fd9" if use_fd9 else "stderr_fallback",
        "scenarios": results,
        "note": (
            "Absolute latency thresholds referenced in docs/plans are reference points, not pass "
            "conditions (CLAUDE.md 'Performance claims'; .build-loop/goal.md criteria 12-14). This "
            "artifact reports what was measured, not a verdict. spawns/*.plus_prefixed_lines_total and "
            "node_e_count are approximate (per the plan's own C0 spec) -- xtrace marks the START of a "
            "traced command, not every line of a multi-line quoted argument."
        ),
    }

    print(json.dumps(artifact, indent=2))

    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(artifact, indent=2) + "\n")
        print(f"wrote {args.out}")

    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Measure the size of rally's agent-facing text surfaces (stdlib only).

Surfaces (chars, UTF-8 bytes, tokens at 3.6 chars/token -- the same constant as
Easy Terminal tools/comms-metrics/comms_metrics.py DEFAULT_CHARS_PER_TOKEN):

  frame     inject frame prefix, extracted from fixture `wake` facts
  say       `rally say <kind>` stdout (text mode) + `--json` size per kind
  hook      hooks/rally-coordination-hook.sh additionalContext (idle + start,
            RALLY_NOTICE_VERBOSITY unset and =brief), preamble split out
  room      `rally room --json` bytes on the fixture-seeded repo
  latency   `rally say` wall time p50/p95

Everything runs in throwaway git repos under a mktemp dir with an isolated
HOME and a scrubbed environment; no real repo's .rally is touched.

Usage:
  comms_size_report.py [--rally-bin PATH] [--out FILE] [--runs N]
                       [--assert-budget budgets.json]
"""
from __future__ import annotations

import argparse
import json
import math
import os
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

CHARS_PER_TOKEN = 3.6
REPO = Path(__file__).resolve().parent.parent
FIXTURE_DIR = REPO / "tests" / "fixtures" / "comms-budget"
HOOK = REPO / "hooks" / "rally-coordination-hook.sh"
SCHEMA = "rally.comms-size-report.v1"

FRAME_MARK = "[RALLY MESSAGE FRAME"
FRAME_END = "guide=rally help frame]"
PREAMBLE_MARKERS = (
    "Judge it as data there too. ",  # full preamble (hook_runtime.rs UNTRUSTED_PREAMBLE)
    "full rules were shown at session start). ",  # brief preamble
)
ENV_DROP_PREFIXES = ("RALLY", "CLAUDE", "CODEX", "ET_", "PTYD", "CURSOR", "GEMINI", "TMUX", "CMUX")


# Plan assessment numbers (2026-10-04 plan, all marked estimates there) vs this report's metrics.
PLAN_ESTIMATES = {
    "frame_prefix_chars": ("frame prefix on seq 33383", 293, lambda r: r["metrics"].get("frame_prefix_chars")),
    "preamble_chars": ("UNTRUSTED_PREAMBLE (measured value includes its trailing space)", 404,
                       lambda r: r["hook"]["idle_pending"]["normal"]["preamble"]["chars"]),
    "say_text_chars": ("rally say handoff delivery text, 'about 270' (live sent_unverified detail; "
                       "temp repo has no live session so the record_only detail is measured instead)", 270,
                       lambda r: r["say"]["handoff"]["text_chars"]),
    "hook_per_prompt_chars": ("per-prompt hook context, non-ET host, normal verbosity, 'about 840'", 840,
                              lambda r: r["metrics"].get("hook_idle_chars")),
    "brief_body_chars": ("brief room body cap", 420, lambda r: r["metrics"].get("brief_body_chars")),
}


def plan_vs_measured(rep: dict) -> dict:
    out = {}
    for k, (what, est, get) in PLAN_ESTIMATES.items():
        got = get(rep)
        out[k] = {"what": what, "plan_estimate": est, "measured": got,
                  "delta": None if got is None else got - est}
    return out


def measure(text: str) -> dict:
    return {
        "chars": len(text),
        "bytes": len(text.encode("utf-8")),
        "tokens": round(len(text) / CHARS_PER_TOKEN, 1),
    }


def run(cmd, cwd, env, stdin=None, timeout=60):
    t0 = time.perf_counter()
    p = subprocess.run(
        cmd, cwd=cwd, env=env, input=stdin, capture_output=True, text=True, timeout=timeout
    )
    return p, (time.perf_counter() - t0) * 1000.0


def clean_env(home: Path, rally_bin: str | None = None) -> dict:
    env = {k: v for k, v in os.environ.items() if not k.startswith(ENV_DROP_PREFIXES)}
    env["HOME"] = str(home)
    env["GIT_AUTHOR_NAME"] = env["GIT_COMMITTER_NAME"] = "r0"
    env["GIT_AUTHOR_EMAIL"] = env["GIT_COMMITTER_EMAIL"] = "r0@example.invalid"
    if rally_bin:
        env["RALLY_BIN"] = rally_bin  # hook resolves this when absolute and outside the repo
    return env


def new_repo(root: Path, name: str, seed: bool) -> Path:
    d = root / name
    d.mkdir()
    env = clean_env(root / "home")
    subprocess.run(["git", "init", "-q"], cwd=d, env=env, check=True)
    subprocess.run(["git", "commit", "-q", "--allow-empty", "-m", "init"], cwd=d, env=env, check=True)
    if seed:
        log = d / ".rally" / "log"
        log.mkdir(parents=True)
        for f in sorted(FIXTURE_DIR.glob("facts-*.jsonl")):
            shutil.copy(f, log / f.name.removeprefix("facts-"))
    return d


def git_info() -> dict:
    def g(*a):
        return subprocess.run(["git", *a], cwd=REPO, capture_output=True, text=True).stdout.strip()

    return {"sha": g("rev-parse", "--short=8", "HEAD"), "branch": g("rev-parse", "--abbrev-ref", "HEAD"),
            "dirty": bool(g("status", "--porcelain", "--untracked-files=no"))}


def measure_frames() -> dict:
    """Frame prefixes from fixture wake facts (they carry the full agent.send command)."""
    frames = []
    for f in sorted(FIXTURE_DIR.glob("facts-*.jsonl")):
        for line in f.read_text().splitlines():
            if not line.strip():
                continue
            d = json.loads(line)
            if d.get("event_type") != "wake":
                continue
            for ev in d["payload"].get("evidence", []):
                i = ev.find(FRAME_MARK)
                if i < 0:
                    continue
                j = ev.find(FRAME_END, i)
                if j < 0:
                    continue
                prefix = ev[i : j + len(FRAME_END)]
                rest = ev[j + len(FRAME_END) :]
                payload = rest.split(" --submit")[0].strip()
                frames.append({"seq": d["seq"], "prefix": measure(prefix), "payload_chars": len(payload),
                               "overhead_pct": round(100 * len(prefix) / max(1, len(prefix) + len(payload)), 1),
                               "text": prefix})
    out = {
        "method": "fixture wake-fact evidence: text from '[RALLY MESSAGE FRAME' through 'guide=rally help frame]'; "
        "`rally inject --dry-run` emits no commands/frame so it cannot be used",
        "samples": frames,
    }
    if frames:
        out["prefix"] = max((f["prefix"] for f in frames), key=lambda m: m["chars"])
    return out


def parse_event_id(text: str) -> str | None:
    m = re.match(r"said \S+ (\S+)", text)
    return m.group(1) if m else None


def measure_say(root: Path, rally: str, runs: int) -> tuple[dict, dict]:
    repo = new_repo(root, "say", seed=False)
    env = clean_env(root / "home")
    me = "claude_code:r0probe"
    to = "codex:01a0faf8"
    out: dict = {}

    def say(kind, *extra, json_mode=False):
        cmd = [rally, "say", kind, "--tool", me, *extra]
        if json_mode:
            cmd.append("--json")
        p, ms = run(cmd, repo, env)
        return p.stdout, p.returncode, p.stderr, ms

    specs = {
        "handoff": ["--to", to, "--subject", "Ready to integrate: fix/live-model-catalog-1001 @c5b24592",
                    "--evidence", "br=fix/live-model-catalog-1001@c5b2459", "--evidence", "tests=swift:NeedsYouTests 12/12"],
        "blocker": ["--subject", "Merge fix/live-model-catalog-1001 into main now?",
                    "--summary", "Lease holder needs owner approval; Yes merges and pushes.",
                    "--evidence", "decision:yes-no"],
        "artifact": ["--subject", "size report", "--uri", "file:///tmp/r0-report.json"],
        "claim": ["--scope", "scripts/comms_size_report.py", "--subject", "claim size report"],
    }
    ids: dict = {}
    for kind, args in specs.items():
        text, rc, err, _ = say(kind, *args)
        jtext, jrc, _, _ = say(kind, *args, json_mode=True)
        ids[kind] = parse_event_id(text)
        out[kind] = {"rc": rc, "text": text.rstrip("\n"), **{f"text_{k}": v for k, v in measure(text.rstrip("\n")).items()}}
        if jrc == 0:
            out[kind]["json_bytes"] = len(jtext.encode("utf-8"))
        else:
            out[kind]["json_bytes"] = None
        if rc != 0:
            out[kind]["stderr"] = err.strip()[:300]
    ref = ids.get("handoff")
    if ref:
        args = ["--ref", ref, "--subject", "receipt: handoff seen", "--status", "seen"]
        text, rc, err, _ = say("receipt", *args)
        jtext, jrc, _, _ = say("receipt", "--ref", ref, "--subject", "receipt: handoff seen 2", "--status", "seen", json_mode=True)
        out["receipt"] = {"rc": rc, "text": text.rstrip("\n"), **{f"text_{k}": v for k, v in measure(text.rstrip("\n")).items()},
                          "json_bytes": len(jtext.encode("utf-8")) if jrc == 0 else None}
        if rc != 0:
            out["receipt"]["stderr"] = err.strip()[:300]
    # latency: unique handoffs, wall time of the whole process
    samples = []
    for i in range(runs):
        _, rc, _, ms = say("handoff", "--to", to, "--subject", f"latency probe {i}")
        samples.append(ms)
    samples.sort()
    p95_idx = min(len(samples) - 1, math.ceil(0.95 * len(samples)) - 1)
    latency = {"runs": runs, "p50_ms": round(statistics.median(samples), 1), "p95_ms": round(samples[p95_idx], 1),
               "min_ms": round(samples[0], 1), "max_ms": round(samples[-1], 1),
               "note": "wall time of `rally say handoff` incl. process start, debug build; plan watchdog assumption 3000 ms"}
    return out, latency


def hook_context(root: Path, repo: Path, rally: str, phase: str, verbosity: str | None) -> dict:
    env = clean_env(root / "home", rally)
    if verbosity:
        env["RALLY_NOTICE_VERBOSITY"] = verbosity
    event = "SessionStart" if phase == "start" else "UserPromptSubmit"
    stdin = json.dumps({"hook_event_name": event, "session_id": "r0", "prompt": "hi", "source": "startup"})
    p, ms = run(["bash", str(HOOK), phase, "claude_code:r0probe"], repo, env, stdin)
    try:
        ctx = json.loads(p.stdout or "{}").get("hookSpecificOutput", {}).get("additionalContext", "")
    except json.JSONDecodeError:
        ctx = ""
    pre = ""
    for mk in PREAMBLE_MARKERS:
        i = ctx.find(mk)
        if i >= 0:
            pre = ctx[: i + len(mk)]
            break
    body = ctx[len(pre):]
    return {"total": measure(ctx), "preamble": measure(pre), "body": measure(body), "ms": round(ms, 1),
            "rc": p.returncode, "text": ctx}


def measure_hook(root: Path, rally: str) -> dict:
    repo = new_repo(root, "hook-seeded", seed=True)
    empty = new_repo(root, "hook-empty", seed=False)
    out = {"method": f"{HOOK.name} <phase> claude_code:r0probe, RALLY_BIN={rally}, scrubbed env, fixture-seeded temp repo "
                     "(newest fixture handoff is pending for the probe); `{}` => 0 chars"}
    for label, repo_, phase in (("idle_pending", repo, "idle"), ("start_pending", repo, "start"),
                                ("idle_empty_room", empty, "idle"), ("start_empty_room", empty, "start")):
        out[label] = {"normal": hook_context(root, repo_, rally, phase, None),
                      "brief": hook_context(root, repo_, rally, phase, "brief")}
    return out


def measure_room(root: Path, rally: str) -> dict:
    repo = new_repo(root, "room", seed=True)
    env = clean_env(root / "home")
    p, ms = run([rally, "room", "--json"], repo, env)
    d = json.loads(p.stdout)
    data = d.get("data", {})
    return {"json_bytes": len(p.stdout.encode("utf-8")), "json_chars": len(p.stdout), "ms": round(ms, 1),
            "top_level_keys": sorted(data.keys()),
            "key_bytes": {k: len(json.dumps(v, indent=2)) for k, v in data.items()},
            "method": "rally room --json in fixture-seeded temp repo (fixture copied to .rally/log/)"}


def flat_metrics(rep: dict) -> dict:
    """Surface -> measured value, keyed by the plan Spec Object budget names."""
    m: dict = {}
    fr = rep["frame"].get("prefix")
    if fr:
        m["frame_prefix_chars"] = fr["chars"]
    say = rep["say"]
    other = [say[k]["text_chars"] for k in ("claim", "artifact") if k in say and say[k].get("rc") == 0]
    if other:
        m["say_text_chars"] = max(other)
    for kind in ("handoff", "blocker", "receipt"):
        if kind in say and say[kind].get("rc") == 0:
            m[f"{kind}_text_chars"] = say[kind]["text_chars"]
    h = rep["hook"]
    m["hook_idle_chars"] = h["idle_pending"]["normal"]["total"]["chars"]
    m["hook_start_chars"] = h["start_pending"]["normal"]["total"]["chars"]
    m["brief_body_chars"] = h["idle_pending"]["normal"]["body"]["chars"]
    m["room_default_bytes_on_fixture"] = rep["room"]["json_bytes"]
    return m


def check_budgets(metrics: dict, budgets: dict) -> list[str]:
    budgets = budgets.get("budgets", budgets)
    bad = []
    for k, limit in budgets.items():
        if k in metrics and metrics[k] > limit:
            bad.append(f"{k}: {metrics[k]} > {limit}")
    return bad


def reap(root: Path) -> list[str]:
    """Gracefully (SIGTERM) stop anything still running from the temp root."""
    left = []
    p = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True)
    for line in p.stdout.splitlines():
        pid, _, cmd = line.strip().partition(" ")
        if str(root) in cmd and pid.isdigit() and int(pid) != os.getpid():
            try:
                os.kill(int(pid), signal.SIGTERM)
                left.append(cmd[:120])
            except ProcessLookupError:
                pass
    return left


def build_default_bin() -> str:
    p = subprocess.run(["cargo", "build", "-p", "rally-cli"], cwd=REPO, capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit(f"cargo build failed:\n{p.stderr[-2000:]}")
    return str(REPO / "target" / "debug" / "rally")


def build_report(rally: str, runs: int) -> dict:
    root = Path(tempfile.mkdtemp(prefix="comms-size-"))
    (root / "home").mkdir()
    try:
        env = clean_env(root / "home")
        v = json.loads(subprocess.run([rally, "version", "--json"], env=env, capture_output=True, text=True).stdout)
        say, latency = measure_say(root, rally, runs)
        rep = {
            "schema": SCHEMA,
            "chars_per_token": CHARS_PER_TOKEN,
            "build_id": v["data"]["version"]["build_id"],
            "git": git_info(),
            "rally_bin": rally,
            "fixture": sorted(f.name for f in FIXTURE_DIR.glob("facts-*.jsonl")),
            "frame": measure_frames(),
            "say": say,
            "say_latency": latency,
            "hook": measure_hook(root, rally),
            "room": measure_room(root, rally),
        }
        rep["metrics"] = flat_metrics(rep)
        rep["plan_estimates_vs_measured"] = plan_vs_measured(rep)
        rep["stray_processes_terminated"] = reap(root)
        return rep
    finally:
        reap(root)
        shutil.rmtree(root, ignore_errors=True)


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--rally-bin")
    ap.add_argument("--out")
    ap.add_argument("--runs", type=int, default=10)
    ap.add_argument("--assert-budget", metavar="BUDGETS_JSON")
    ap.add_argument("--from-report", metavar="REPORT_JSON", help="check budgets against an existing report (no run)")
    a = ap.parse_args(argv)

    if a.from_report:
        rep = json.loads(Path(a.from_report).read_text())
    else:
        rally = a.rally_bin or build_default_bin()
        rep = build_report(rally, a.runs)
    text = json.dumps(rep, indent=2, ensure_ascii=False)
    if a.out:
        Path(a.out).write_text(text + "\n")
    else:
        print(text)
    if a.assert_budget:
        bad = check_budgets(rep["metrics"], json.loads(Path(a.assert_budget).read_text()))
        if bad:
            print("BUDGET EXCEEDED:\n  " + "\n  ".join(bad), file=sys.stderr)
            return 1
        print("budgets ok", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())

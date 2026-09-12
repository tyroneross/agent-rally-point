#!/usr/bin/env python3
"""Real-tmux causal route-proof controls; no model or fresh-install claims.

Run: python3 tests/runtime/test_route_probe_transport.py --binary /path/to/rally
Requires installed tmux. Each case has a private HOME, repository and tmux server.
Exit 0 means every positive/negative control passed; missing tools exit 2.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time


TOKEN_RE = re.compile(r"RALLY_ROUTE_DELIVERY_[0-9a-f]{32}")
HANDOFF_RE = re.compile(r"handoff (fact_[A-Za-z0-9_-]+)")
RECEIVER = "probe:receiver"
MODES = ("consume", "poll", "no_ack", "wrong_token", "wrong_session")


def bounded(command, cwd, env, timeout=45):
    """Kill only this invocation's process group when the deadline expires."""
    process = subprocess.Popen(command, cwd=cwd, env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.communicate(timeout=5)
        raise RuntimeError("command timed out: " + " ".join(command[:3]))
    return {"exit": process.returncode, "stdout": stdout, "stderr": stderr}


def objects(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from objects(child)
    elif isinstance(value, list):
        for child in value:
            yield from objects(child)


def facts(root):
    for path in (root / ".rally/log").glob("*.jsonl"):
        for line in path.read_text().splitlines():
            try:
                yield from objects(json.loads(line))
            except ValueError:
                # Another process may be appending the final record.
                continue


def snapshot(root):
    """Capture public ledger/inbox bytes before the receiver writes an ACK."""
    return {str(path.relative_to(root)): path.read_text(errors="replace")
            for directory in (root / ".rally", root / "home")
            for path in directory.rglob("*.jsonl") if path.is_file()}


def write_json(path, value):
    # Atomic readiness/evidence so the controller never reads half a document.
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def receiver(binary, root, mode):
    import tty

    def reply(fact, token):
        metadata = next(json.loads(item.split(":", 1)[1])
                        for item in fact.get("evidence", [])
                        if item.startswith("route-probe-v1:"))
        before = snapshot(root)
        write_json(root / "observation.json", {
            "handoff": fact["event_id"], "metadata": metadata,
            "received_token": token, "pre_response_jsonl": before,
        })
        if mode == "no_ack":
            return
        evidence = token
        if mode in ("poll", "wrong_token"):
            # Even full ledger access supplies only a digest, not the challenge.
            evidence = "RALLY_ROUTE_DELIVERY_" + "0" * 32
            if evidence == token:
                evidence = "RALLY_ROUTE_DELIVERY_" + "1" * 32
        epoch = metadata["lead_epoch"]
        command = [binary, "say", "artifact", "--tool", RECEIVER,
                   "--ref", fact["event_id"], "--subject", "Controlled " + mode + " receipt",
                   "--evidence", metadata["nonce"], "--evidence", evidence,
                   "--evidence", "lead-epoch:" + (str(epoch) if epoch is not None else "none"),
                   "--json"]
        if metadata.get("role"):
            command += ["--role", metadata["role"]]
        env = dict(os.environ)
        if mode == "wrong_session":
            env["RALLY_SESSION_ID"] = "intentionally-different-receiver-session"
        result = bounded(command, root, env)
        result["attempted_token"] = evidence
        write_json(root / "reply.json", result)

    if mode != "poll":
        tty.setraw(0)
    (root / "receiver-ready").touch()
    seen = set()
    buffer = ""
    while True:
        if mode == "poll":
            for fact in facts(root):
                if (fact.get("kind") == "handoff" and fact.get("target") == RECEIVER
                        and fact.get("event_id") not in seen
                        and any(item.startswith("route-probe-v1:")
                                for item in fact.get("evidence", []))):
                    seen.add(fact["event_id"])
                    reply(fact, None)
            time.sleep(0.025)
        else:
            data = os.read(0, 65536)
            if not data:
                return
            with (root / "received").open("ab") as output:
                output.write(data)
            buffer = (buffer + data.decode(errors="replace"))[-131072:]
            token = TOKEN_RE.search(buffer)
            handoff = HANDOFF_RE.search(buffer)
            if token and handoff and handoff[1] not in seen:
                fact = next((f for f in facts(root) if f.get("event_id") == handoff[1]), None)
                if fact is not None:
                    seen.add(handoff[1])
                    reply(fact, token[0])


def run_case(binary, tmux, mode, probe_timeout):
    root = Path(tempfile.mkdtemp(prefix="rpt-", dir="/tmp"))
    (root / ".git").mkdir()
    (root / "home").mkdir()
    # Never inherit an existing room, managed identity, daemon or tmux socket.
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("RALLY_", "GITHUB_", "XDG_"))
           and key not in ("TMUX", "TMUX_PANE", "TMUX_TMPDIR")}
    env.update(HOME=str(root / "home"), TMUX_TMPDIR=str(root),
               PATH=str(Path(tmux).parent) + os.pathsep + os.defpath,
               RALLY_SESSION_ID="transport-test-lead", RALLY_HOOKS="off",
               RALLY_DAEMON_AUTOSTART="0", RALLY_HOOK_TIMEOUT_MS="30000",
               RALLY_SETUP_STATE_DIR=str(root / "setup"))
    result = {"mode": mode, "expected_ready": mode == "consume", "passed": False}

    def call(args, timeout=45):
        output = bounded([binary, *args, "--json"], root, env, timeout)
        output["body"] = json.loads(output["stdout"])
        return output

    try:
        result["launch"] = call([
            "run", "rosslabs-agent-harness", "--name", "transport-test",
            "--tool", RECEIVER, "--shared", "--backend", "tmux", "--tmux-bin", tmux,
            "--command-json", json.dumps([sys.executable, str(Path(__file__).resolve()),
                                          "--receiver", binary, str(root), mode]),
        ])
        assert result["launch"]["exit"] == 0, "managed receiver launch failed"
        deadline = time.monotonic() + 30
        while not (root / "receiver-ready").exists() and time.monotonic() < deadline:
            time.sleep(0.025)
        assert (root / "receiver-ready").exists(), "receiver never became ready"
        result["probe"] = call(["routes", "--probe", RECEIVER, "--tool", "probe:lead",
                                "--timeout-seconds", str(probe_timeout)], probe_timeout + 45)
        routes = result["probe"]["body"]["data"]["routes"]
        route = next(item for item in routes["items"] if item["actor"] == RECEIVER)
        injection = routes["probe"]["injection"]
        result.update(ready=route["state"] == "ready",
                      live_prompt=route["capabilities"]["live_prompt"],
                      delivery_state=injection["delivery_state"], ack_state=injection["ack_state"])
        # A broken backend or receiver is a failed control, never a passing negative.
        assert injection["delivery_state"] in ("sent_unverified", "delivered"), "transport did not write"
        observation_path = root / "observation.json"
        assert observation_path.exists(), "receiver did not process the challenge/handoff"
        observation = json.loads(observation_path.read_text())
        result["observation"] = observation
        token = observation["received_token"]
        assert observation["metadata"].get("delivery_digest"), "handoff lacks delivery digest"
        leaked = [path for path, text in observation["pre_response_jsonl"].items()
                  if TOKEN_RE.search(text)]
        result["pre_response_token_leaks"] = leaked
        assert not leaked, "raw transport challenge leaked into ledger/directive: " + repr(leaked)
        received = (root / "received").read_bytes() if (root / "received").exists() else b""
        result["received_bytes"] = len(received)
        result["received_text"] = received.decode(errors="replace")
        if mode == "poll":
            assert not received and token is None, "poller consumed terminal input"
        else:
            assert received and token, "receiver did not consume actual terminal challenge"
            assert hashlib.sha256(token.encode()).hexdigest() == observation["metadata"]["delivery_digest"], "received token does not match handoff digest"
        if mode != "no_ack":
            deadline = time.monotonic() + 5
            while not (root / "reply.json").exists() and time.monotonic() < deadline:
                time.sleep(0.025)
            assert (root / "reply.json").exists(), "receiver did not finish ACK attempt"
            result["reply"] = json.loads((root / "reply.json").read_text())
            if mode != "wrong_session":
                assert result["reply"]["exit"] == 0, "ACK control failed to commit artifact"
            else:
                # Session mismatch may be rejected at write or proof verification.
                assert result["reply"]["attempted_token"] == token, "wrong-session control lacks real token"
        else:
            assert not (root / "reply.json").exists(), "silent receiver unexpectedly replied"
        expected = result["expected_ready"]
        assert result["ready"] == expected and result["live_prompt"] == expected, "incorrect route readiness"
        assert result["probe"]["exit"] == (0 if expected else 4), "incorrect route exit status"
        result["passed"] = True
    except Exception as error:
        result["error"] = repr(error)
    finally:
        # Preserve complete ledger/inbox evidence before removing the isolated HOME.
        try:
            result["final_jsonl"] = snapshot(root)
            result["final_pane"] = bounded([tmux, "capture-pane", "-p"], root, env, 10)
        except Exception as error:
            result["passed"] = False
            result["evidence_error"] = repr(error)
        try:
            result["server_stop"] = bounded([tmux, "kill-server"], root, env, 10)
            if result["server_stop"]["exit"] != 0:
                result["passed"] = False
                result["cleanup_error"] = "private tmux server did not stop cleanly"
        except Exception as error:
            result["passed"] = False
            result["cleanup_error"] = repr(error)
        shutil.rmtree(root)
    return result


def main():
    if len(sys.argv) == 5 and sys.argv[1] == "--receiver":
        receiver(sys.argv[2], Path(sys.argv[3]), sys.argv[4])
        return 0
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", type=Path, help="write complete JSON evidence (also printed to stdout)")
    parser.add_argument("--probe-timeout", type=int, default=8)
    args = parser.parse_args()
    if not 1 <= args.probe_timeout <= 120:
        parser.error("--probe-timeout must be between 1 and 120 seconds")
    binary = str(args.binary.resolve())
    tmux = shutil.which("tmux")
    result = {"binary": binary, "tmux": tmux, "model_evidence": False, "passed": False, "cases": []}
    if not tmux or not os.access(binary, os.X_OK):
        result["unavailable"] = "Requires executable --binary and installed tmux; no test was passed"
        status = 2
    else:
        result["binary_sha256"] = hashlib.sha256(Path(binary).read_bytes()).hexdigest()
        for mode in MODES:
            case = run_case(binary, tmux, mode, args.probe_timeout)
            result["cases"].append(case)
            print(json.dumps({key: case.get(key) for key in ("mode", "passed", "ready", "error")}),
                  file=sys.stderr, flush=True)
        result["passed"] = all(case["passed"] for case in result["cases"])
        status = 0 if result["passed"] else 1
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        write_json(args.output, result)
    print(json.dumps(result, indent=2))
    return status


if __name__ == "__main__":
    sys.exit(main())

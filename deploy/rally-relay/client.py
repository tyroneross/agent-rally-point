# SPDX-License-Identifier: Apache-2.0
"""Outbound Rally relay client for Macs and unattended workers."""
import argparse
import http.client
import json
import os
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from urllib.parse import quote, urlsplit


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def request(action, body):
    base = os.environ["RALLY_RELAY_URL"].rstrip("/")
    addr = urlsplit(base)
    if addr.scheme != "https" and not (addr.scheme == "http" and addr.hostname in {"localhost", "127.0.0.1", "::1"}):
        raise ValueError("use HTTPS for remote relays")
    room = quote(os.environ["RALLY_ROOM"], safe=":")
    headers = {"Authorization": "Bearer " + os.environ["RALLY_RELAY_TOKEN"], "Content-Type": "application/json"}
    req = urllib.request.Request(f"{base}/rooms/{room}/{action}", json.dumps(body).encode(), headers)
    with urllib.request.build_opener(NoRedirect).open(req, timeout=20) as response:
        return json.load(response)


def work(args):
    # Preserve the outcome before uploading. A crash during the handler requires
    # inspection because repeating an arbitrary handler could repeat its effects.
    if args.capability == "handoff":
        raise ValueError("the addressed agent must acknowledge handoffs explicitly")
    with sqlite3.connect(args.journal) as db:
        db.execute("CREATE TABLE IF NOT EXISTS runs (key TEXT PRIMARY KEY, result TEXT)")
        while True:
            try:
                item = request("claim", {"capability": args.capability})
                if item:
                    event = item["envelope"]["event_id"]
                    key = json.dumps([os.environ["RALLY_RELAY_URL"], os.environ["RALLY_ROOM"], item["owner"], event])
                    cursor = db.execute("INSERT OR IGNORE INTO runs(key) VALUES (?)", (key,))
                    db.commit()
                    if cursor.rowcount:
                        env = dict(os.environ)
                        env.pop("RALLY_RELAY_TOKEN", None)
                        with tempfile.TemporaryFile() as output:
                            try:
                                with subprocess.Popen(args.handler, stdin=subprocess.PIPE, stdout=output,
                                                      stderr=subprocess.STDOUT, env=env, start_new_session=True) as run:
                                    try:
                                        run.communicate(json.dumps(item["envelope"]).encode(), timeout=args.timeout)
                                    except subprocess.TimeoutExpired:
                                        os.killpg(run.pid, signal.SIGKILL)
                                        run.wait()
                                        output.write(b"handler timed out")
                                    code = run.returncode
                            except OSError as exc:
                                output.write(str(exc).encode())
                                code = 1
                            output.seek(0)
                            result = json.dumps({"exit": code, "output": output.read(2000).decode(errors="replace")}, ensure_ascii=False)
                        db.execute("UPDATE runs SET result=? WHERE key=?", (result, key))
                        db.commit()
                    else:
                        result, = db.execute("SELECT result FROM runs WHERE key=?", (key,)).fetchone()
                        if result is None:
                            raise RuntimeError(f"inspect interrupted run {event} before resuming")
                    request("ack", {"id": event})
                    request("complete", {"id": event, "result": result})
            except urllib.error.HTTPError as exc:
                if exc.code < 500 or args.once:
                    raise
            except (urllib.error.URLError, TimeoutError, http.client.RemoteDisconnected):
                if args.once:
                    raise
            if args.once:
                return
            time.sleep(10)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    cmds = parser.add_subparsers(dest="action", required=True)
    send = cmds.add_parser("send")
    for name in ("id", "target", "text"):
        send.add_argument(name)
    send.add_argument("--capability", default="handoff")
    cmds.add_parser("members")
    for action in ("claim", "wait"):
        cmds.add_parser(action).add_argument("--capability", default="handoff")
    for action in ("status", "ack", "complete"):
        cmd = cmds.add_parser(action)
        cmd.add_argument("id")
        if action == "complete":
            cmd.add_argument("result")
    cmd = cmds.add_parser("work")
    cmd.add_argument("--capability", required=True)
    cmd.add_argument("--journal", required=True)
    cmd.add_argument("--once", action="store_true")
    cmd.add_argument("--timeout", type=int, default=600)
    cmd.add_argument("handler", nargs="+")
    args = parser.parse_args()
    if args.action == "work":
        work(args)
    elif args.action == "wait":
        while True:
            try:
                item = request("claim", {"capability": args.capability})
                if item is not None:
                    print(json.dumps(item, indent=2))
                    return
            except urllib.error.HTTPError as exc:
                if exc.code < 500:
                    raise
            except (urllib.error.URLError, TimeoutError, http.client.RemoteDisconnected):
                pass
            time.sleep(10)
    else:
        print(json.dumps(request(args.action, {k:v for k,v in vars(args).items() if k != "action"}), indent=2))


if __name__ == "__main__":
    try:
        main()
    except urllib.error.HTTPError as error:
        sys.exit(f"relay HTTP {error.code}: {error.read(500).decode(errors='replace')}")

#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""agent-rally-point CLI."""
from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

from .changes import new_event_id
from .coordination_trace import (
    event_label,
    filter_since,
    find_record,
    format_time,
    load_records,
    parse_since,
    pending_handoffs,
    record_id,
    related_records,
)
from .discover import discover
from .post import post


def _channel_dir(args: argparse.Namespace) -> tuple[Path, str]:
    if getattr(args, "channel_dir", None):
        return Path(args.channel_dir).expanduser(), Path.cwd().name
    info = discover(Path(getattr(args, "workdir", None) or Path.cwd()))
    channel = info.get("channel_dir")
    if not channel:
        raise SystemExit("agent-rally-point: no channel_dir resolved for this repo")
    return Path(channel), info.get("app_slug") or Path.cwd().name


def _tool(args: argparse.Namespace) -> str | None:
    value = getattr(args, "tool", None) or os.environ.get("AGENT_RALLY_TOOL") or os.environ.get("APP_PULSE_TOOL")
    return value or None


def _print_timeline(records: list[dict], *, show_ids: bool = False) -> None:
    if not records:
        print("No trace events found.")
        return
    for rec in records:
        prefix = f"{format_time(rec)}  r{rec.get('revision', '?'):>4}"
        if show_ids:
            prefix += f"  {record_id(rec)}"
        print(f"{prefix}  {event_label(rec)}")


def cmd_report(args: argparse.Namespace) -> int:
    channel, _slug = _channel_dir(args)
    cutoff = parse_since(args.since)
    records = filter_since(load_records(channel), cutoff)
    print(f"Trace report: {channel}")
    if args.since:
        print(f"Window: since {args.since}")
    print()
    _print_timeline(records[-args.limit:], show_ids=args.ids)
    print()
    pending = pending_handoffs(records, tool=_tool(args))
    print(f"Pending handoffs: {len(pending)}")
    for item in pending:
        age = f", age={item.age_seconds}s" if item.age_seconds is not None else ""
        files = f", files={','.join(item.files)}" if item.files else ""
        print(f"- {item.event_id} from={item.from_tool} to={item.to_tool}: {item.subject}{age}{files}")
    return 0


def cmd_replay(args: argparse.Namespace) -> int:
    channel, _slug = _channel_dir(args)
    records = load_records(channel)
    if args.thread:
        records = related_records(records, args.thread)
    else:
        records = filter_since(records, parse_since(args.since))
    print(f"Trace replay: {channel}")
    if args.thread:
        print(f"Thread: {args.thread}")
    elif args.since:
        print(f"Window: since {args.since}")
    print()
    _print_timeline(records[-args.limit:], show_ids=True)
    return 0


def cmd_thread(args: argparse.Namespace) -> int:
    channel, _slug = _channel_dir(args)
    records = related_records(load_records(channel), args.identifier)
    print(f"Thread: {args.identifier}")
    print(f"Channel: {channel}")
    print()
    _print_timeline(records, show_ids=True)
    return 0


def cmd_inbox(args: argparse.Namespace) -> int:
    channel, _slug = _channel_dir(args)
    records = filter_since(load_records(channel), parse_since(args.since))
    pending = pending_handoffs(records, tool=_tool(args))
    if not pending:
        print("Inbox empty.")
        return 0
    for item in pending:
        age = f" age={item.age_seconds}s" if item.age_seconds is not None else ""
        print(f"{item.event_id} from={item.from_tool} to={item.to_tool}{age}")
        print(f"  {item.subject}")
        if item.files:
            print(f"  files: {', '.join(item.files)}")
    return 0


def cmd_handoff(args: argparse.Namespace) -> int:
    channel, app_slug = _channel_dir(args)
    from_tool = args.from_tool or _tool(args) or "unknown"
    event_id = new_event_id()
    payload = {
        "from_tool": from_tool,
        "to_tool": args.to,
        "subject": args.subject,
        "requires_ack": not args.no_ack,
    }
    if args.files:
        payload["ref_files"] = args.files
    if args.notes:
        payload["notes"] = args.notes
    rev = post(
        channel_dir=channel,
        kind="handoff",
        tool=from_tool,
        model=args.model or "unknown",
        run_id=args.run_id or "agent-rally-cli",
        app_slug=app_slug,
        payload=payload,
        event_id=event_id,
        subject=args.subject,
    )
    if rev is None:
        print("failed to post handoff", file=sys.stderr)
        return 1
    print(f"posted handoff {event_id} revision={rev} to={args.to}")
    return 0


def _ack(args: argparse.Namespace, verdict: str) -> int:
    channel, app_slug = _channel_dir(args)
    records = load_records(channel)
    target = find_record(records, args.identifier)
    tool = _tool(args) or args.tool or "unknown"
    payload = {"ref_handoff_id": args.identifier, "verdict": verdict}
    if getattr(args, "summary", None):
        payload["summary"] = args.summary
    if getattr(args, "reason", None):
        payload["reason"] = args.reason
    causation = target.get("id") if target else args.identifier
    thread = target.get("thread_id") if target else None
    rev = post(
        channel_dir=channel,
        kind="ack",
        tool=tool,
        model=args.model or "unknown",
        run_id=args.run_id or "agent-rally-cli",
        app_slug=app_slug,
        payload=payload,
        causation_id=causation,
        thread_id=thread,
        subject=args.identifier,
    )
    if rev is None:
        print("failed to post ack", file=sys.stderr)
        return 1
    print(f"posted {verdict} ack for {args.identifier} revision={rev}")
    return 0


def cmd_ack(args: argparse.Namespace) -> int:
    return _ack(args, "done")


def cmd_reject(args: argparse.Namespace) -> int:
    return _ack(args, "rejected")


def cmd_needs_info(args: argparse.Namespace) -> int:
    return _ack(args, "needs-info")


def _add_common(p: argparse.ArgumentParser) -> None:
    p.add_argument("--channel-dir", help="read/write a specific channel directory")
    p.add_argument("--workdir", help="repo workdir for discovery; defaults to cwd")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="agent-rally-point")
    sub = parser.add_subparsers(dest="cmd", required=True)

    report = sub.add_parser("report", help="summarize recent coordination trace events")
    _add_common(report)
    report.add_argument("--since", default="24h")
    report.add_argument("--limit", type=int, default=80)
    report.add_argument("--tool")
    report.add_argument("--ids", action="store_true")
    report.set_defaults(func=cmd_report)

    replay = sub.add_parser("replay", help="print an interleaved coordination timeline")
    _add_common(replay)
    replay.add_argument("--since", default="24h")
    replay.add_argument("--thread")
    replay.add_argument("--limit", type=int, default=200)
    replay.set_defaults(func=cmd_replay)

    thread = sub.add_parser("thread", help="show events related to an id/thread")
    _add_common(thread)
    thread.add_argument("identifier")
    thread.set_defaults(func=cmd_thread)

    inbox = sub.add_parser("inbox", help="list open handoffs")
    _add_common(inbox)
    inbox.add_argument("--tool")
    inbox.add_argument("--since", default="7d")
    inbox.set_defaults(func=cmd_inbox)

    handoff = sub.add_parser("handoff", help="post a handoff")
    _add_common(handoff)
    handoff.add_argument("--to", required=True)
    handoff.add_argument("--subject", required=True)
    handoff.add_argument("--from-tool")
    handoff.add_argument("--files", nargs="*")
    handoff.add_argument("--notes")
    handoff.add_argument("--no-ack", action="store_true")
    handoff.add_argument("--model")
    handoff.add_argument("--run-id")
    handoff.set_defaults(func=cmd_handoff)

    ack = sub.add_parser("ack", help="acknowledge a handoff as done")
    _add_common(ack)
    ack.add_argument("identifier")
    ack.add_argument("--summary")
    ack.add_argument("--tool")
    ack.add_argument("--model")
    ack.add_argument("--run-id")
    ack.set_defaults(func=cmd_ack)

    reject = sub.add_parser("reject", help="reject a handoff")
    _add_common(reject)
    reject.add_argument("identifier")
    reject.add_argument("--reason", required=True)
    reject.add_argument("--tool")
    reject.add_argument("--model")
    reject.add_argument("--run-id")
    reject.set_defaults(func=cmd_reject)

    needs = sub.add_parser("needs-info", help="mark a handoff as needing more information")
    _add_common(needs)
    needs.add_argument("identifier")
    needs.add_argument("--reason", required=True)
    needs.add_argument("--tool")
    needs.add_argument("--model")
    needs.add_argument("--run-id")
    needs.set_defaults(func=cmd_needs_info)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())

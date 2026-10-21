#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""Reproduce the per-repo open-queue age threshold in `pickup_threshold.rs`.

READ-ONLY. Every ledger is opened as `file:<db>?mode=ro&immutable=1`, so no
lock is taken and nothing is written. Pass COPIES of ledgers if you can.

Usage:
    python3 -I scripts/research/room_pickup_threshold.py DB_LIST [--names] [--seed 7]

DB_LIST is a text file with one `.rally/facts.db` path per line. Each ledger is
one repo. Without `--names`, repos print as room-01, room-02, ... so the output
can be committed to a public repo.

Rule (same as `open_queue::room_pickup_p95` + `pickup_threshold::room_threshold`):
a pickup is the first non-system resolve / receipt / artifact that refs an
ORIGINAL open-queue handoff (no ref; target empty, null or `all`), excluding the
sender closing its own posting. Threshold = the repo's own nearest-rank p95 of
time-to-pickup, floored at 1 h, once the repo has >= N = 8 pickups; below that
the age signal is off. No other repo's data enters.

Outputs: the bootstrap that chose N, and the per-repo threshold table.
"""
import json, math, random, sqlite3, statistics as st, sys
from datetime import datetime



def ts(raw):
    try:
        return datetime.fromisoformat((raw or "").replace("Z", "+00:00"))
    except ValueError:
        return None


def system(f):
    return f.get("tool") == "rally" or f.get("role") == "system"


LEGACY_CLOSE_KINDS = {"resolve", "receipt", "release", "done", "retract", "handoff.closed"}


def legacy_durations(facts):
    """The 2026-10-18 measurement rule (backlog item AGEN-RALLY-NEXT-m590zk409zjqpzxdevzb3,
    handoff_lifetimes.py): any non-`rally`, non-wake/read reply that refs the
    handoff with a closing kind or a resolved/done/closed/declined status.
    Self-closes count. Kept so the original k inputs reproduce."""
    by_ref = {}
    for f in facts:
        if f.get("ref"):
            by_ref.setdefault(f["ref"], []).append(f)
    out = []
    for h in facts:
        if h.get("kind") != "handoff" or h.get("ref"):
            continue
        if h.get("target") and h.get("target") != "all":
            continue
        t0 = ts(h.get("created_at"))
        if not t0:
            continue
        closes = [
            ts(f.get("created_at"))
            for f in by_ref.get(h["event_id"], [])
            if f.get("tool") != "rally" and f.get("kind") not in ("wake", "read")
            and (f.get("kind") in LEGACY_CLOSE_KINDS or (f.get("status") or "") in ("resolved", "done", "closed", "declined"))
            and ts(f.get("created_at"))
        ]
        if closes:
            out.append((min(closes) - t0).total_seconds() / 3600)
    return out


def room_durations(db, legacy=False):
    con = sqlite3.connect(f"file:{db}?mode=ro&immutable=1", uri=True)
    facts = [json.loads(p) for (p,) in con.execute("select payload from events order by sequence_number")]
    con.close()
    if legacy:
        return legacy_durations(facts)
    originals = {
        f["event_id"]: f
        for f in facts
        if f.get("kind") == "handoff" and not f.get("ref")
        and (f.get("target") or "").strip() in ("", "all")
    }
    first = {}
    for f in facts:
        h = originals.get(f.get("ref") or "")
        if h is None or f.get("kind") not in ("resolve", "receipt", "artifact") or system(f):
            continue
        if f.get("tool") == h.get("tool"):
            continue  # the sender closing its own posting is not a pickup
        at = ts(f.get("created_at"))
        if at:
            first[h["event_id"]] = min(first.get(h["event_id"], at), at)
    out = []
    for hid, closed in first.items():
        opened = ts(originals[hid].get("created_at"))
        if opened and closed >= opened:
            out.append((closed - opened).total_seconds() / 3600)
    return out


def p95(xs):
    xs = sorted(xs)
    return xs[math.ceil(0.95 * len(xs)) - 1]


N_MIN = 8
FLOOR_H = 1.0


def threshold(room_h, n):
    return max(room_h, FLOOR_H) if n >= N_MIN else None


def main():
    args = sys.argv[1:]
    names = "--names" in args
    legacy = "--legacy-close-rule" in args
    seed = int(args[args.index("--seed") + 1]) if "--seed" in args else 7
    dbs = [l.strip() for l in open(args[0]) if l.strip()]
    rooms, seen = {}, set()
    for db in sorted(set(dbs)):
        try:
            d = room_durations(db, legacy)
        except sqlite3.Error as e:
            print(f"skip {db}: {e}", file=sys.stderr)
            continue
        key = tuple(sorted(d))
        if len(d) < 2 or key in seen:
            continue
        seen.add(key)
        rooms[db.split("/.rally")[0]] = d
    ordered = sorted(rooms.items(), key=lambda kv: -len(kv[1]))
    label = {r: (r.rsplit("/", 1)[-1] if names else f"room-{i + 1:02d}") for i, (r, _) in enumerate(ordered)}

    random.seed(seed)
    big = [v for v in rooms.values() if len(v) >= 20]
    print(f"repos={len(rooms)} with >=2 pickups; repos with >=20 pickups (bootstrap base, small): {len(big)}")
    print("\nChoosing N: below 20 pickups the nearest-rank p95 is the slowest pickup seen.")
    print("Bootstrap of that estimate against each large repo's full-history p95:")
    for n in (3, 5, 8, 10, 14, 20):
        below, ratio = [], []
        for v in big:
            true = p95(v)
            for _ in range(2000):
                est = p95(random.choices(v, k=n))
                below.append(est < true); ratio.append(est / true)
        on = sum(1 for v in rooms.values() if len(v) >= n)
        print(f"  N={n:2d}: P(estimate < true p95)={st.mean(below):.2f}  median estimate/true={st.median(ratio):.2f}  repos qualifying={on}/{len(rooms)}")
    qual = [p95(v) for v in rooms.values() if len(v) >= N_MIN]
    print(f"\nFastest qualifying repo p95 (n>={N_MIN}): {min(qual):.1f} h; floor {FLOOR_H:g} h never binds on a measured repo.")

    print(f"\n| Repo | pickups n | own p95 | threshold (N={N_MIN}, floor {FLOOR_H:g} h) |")
    print("|---|---|---|---|")
    for r, v in ordered:
        t = threshold(p95(v), len(v))
        print(f"| {label[r]} | {len(v)} | {p95(v):.1f} h | {'%.0f h' % t if t else 'off (insufficient_history)'} |")


if __name__ == "__main__":
    main()

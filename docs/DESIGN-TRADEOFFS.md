<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Design tradeoffs

Why Agent Rally Point works the way it does. Each section is a decision that had a real
alternative, what got tried, what broke, and what was chosen.

These are not settled truths. They are the current answer, with the evidence that produced it.
If the evidence changes, the answer should.

## 1. Hooks, not a hookless CLI

**The alternative:** ship only the `rally` CLI. Agents call it because their instructions tell
them to. No host hooks, nothing auto-loaded, no committed configuration touching the host.

**What was tried:** that was the original design. Rally was a CLI and a skill. Agents were told
to enter the room, claim before writing, and post artifacts.

**What broke:** compliance was inconsistent and unpredictable. An agent would enter the room at
session start and then never check before a write. Another would claim files but never release
them. A third would do the whole loop correctly for twenty minutes and then drift out of it once
the task got interesting. The instructions were in the skill. The agents had read them. They
still did not reliably run the commands, and the failure was silent — a missed
`check before-write` looks exactly like a repo where nobody else is working.

Coordination that works most of the time is close to useless, because the whole value is knowing
that the absence of a warning means something. If an agent might just not have checked, a clean
check proves nothing.

**What was chosen:** hooks. `SessionStart` registers presence and surfaces room state.
`PreToolUse` checks the write boundary before an edit. The host fires them; the agent's
willingness is not in the loop.

**What it costs:** this is the most intrusive thing Rally does, and it is the source of the most
serious finding in the issue #52 audit. Committed hook registrations mean **opening and trusting
this repo auto-loads code that runs on your host.** That is a real trust decision and it is now
documented as one in [`security/TRUST-MODEL.md`](security/TRUST-MODEL.md) rather than assumed.

The audit's ARP-001 finding was not that hooks are wrong. It was that the hook was doing far more
than surfacing coordination state — it was provisioning a binary: downloading, `chmod +x`,
`cargo install`, writing to `~/.local/bin`. That has been removed. The hook now detects and
advises. Installing is an explicit step a human runs.

The line that came out of that: **a lifecycle hook may observe and inform. It may not acquire or
execute.** Hooks earn their intrusiveness by making compliance near-universal. They lose it the
moment they do anything the user did not ask for.

**Off switches** are first-class, not an afterthought: `RALLY_HOOKS=off` for a session,
`rally hooks off --scope repo` for a repo, `rally hooks status` to see where you stand.

## 2. Rally instructs; it does not manage

**The alternative:** a manager agent. One process that assigns work, tracks who is doing what,
notices when someone stops responding, and reassigns.

**What was tried:** agents self-managing on top of a shared room. Rally records facts, checks
boundaries, routes handoffs, and exposes state. Each agent decides what to do next.

**What broke:** agents left silently. A session would claim three files, work for a while, and
then end — context limit, crash, a human closing a terminal. The claim stayed. The work sat at an
unknown point of completion. Peers saw an active claim on files nobody was touching, and had no
way to tell "working hard" from "gone twenty minutes ago". `rally room` still reports the
accumulated residue of this: open handoffs weeks old, stale facts in the thousands. See RC-008.

**What was chosen:** keep agents self-managing. Fix the observability that made silence
ambiguous, rather than adding an authority.

Three mechanisms came out of it:

- **Mandated polling and check-ins.** Post status every ~10 minutes during long work. Silence
  beyond ~15 minutes is treated as a coordination bug, not as normal. This is in
  `CLAUDE.md` and the skill as a working agreement.
- **Worktree isolation for no-shows.** An agent that goes quiet leaves its work in an isolated
  worktree at a known branch, so a peer can pick up from the last recorded point instead of
  guessing or starting over.
- **Lease expiry on claims.** A claim carries `lease_expires_at`. An expired claim stops
  suppressing other agents.

**Why the manager agent was rejected:** it violates the layer boundary. Rally is infrastructure.
The moment it decides who does what, it becomes a scheduler, and then it needs to model task
semantics, agent capability, and progress — which is the job of the thing being coordinated, not
the coordination substrate. It also becomes a single point of failure with more authority than
anything else in the system, in a design whose entire premise is that there is no server.

Stated as an invariant: **if something would make Rally execute, schedule, retry, or assign, it
belongs in the host or an external runner, not here.**

Admission is deliberately on the other side of that boundary. When a host
explicitly asks to launch or resume a named resource, Rally may atomically grant
or refuse exclusive ownership before the process starts. Rally does not choose
the target, decide when to retry, redirect the UI, or kill the current owner;
those remain host policy. This narrow gate prevents two independent harnesses
from discovering the collision only after both have tried to open the same work
context.

The cost is honest: self-managing agents coordinate worse than a good manager would. This is a
deliberate trade of coordination quality for a boundary that keeps the substrate simple and
unowned.

## 3. Push where possible, pull as the floor

**The alternative:** pick one. Either everything goes through the ledger (simple, universal,
slow), or everything is direct delivery (fast, immediate, requires a live addressable target).

**What was tried:** both, in that order.

Pull-only works everywhere and is genuinely durable — the ledger is a file, it survives process
death, and a peer reads it whenever it next looks. It is also slow in the way that matters: two
agents cannot argue. A disagreement that a human would resolve in three exchanges takes three
turn-boundaries, and by then one of them has moved on.

Push is direct delivery into a live pane — tmux injection, a ptyd socket. It arrives now. Two
agents can actually go back and forth on a design question, which turns out to be where a lot of
the value is. It also fails in more ways: the target must be a live addressable session, the
pane may be gone, the identity may be stale.

**What was chosen: push takes precedence where it is available; pull is the universal fallback,
and pull is what the protocol guarantees.**

Everything durable goes in the ledger regardless. Push is an accelerant on top, never a
substitute — a pushed message that lands is also a fact, and an agent that missed the push finds
it on its next poll.

**What it costs, stated plainly:** the push path currently over-reports. `rally inject` returns
`ok: true` for *enqueue*, which is not *delivery*. RC-001 in the root-cause register documents
this with measurements: at the time of that investigation, 706 of 740 wake facts sat `pending`
and never left that state, and no receiving daemon was running at all. Messages that actually
arrived were mostly hand-carried by the operator or pulled from the ledger by the peer on its
next turn.

So the honest current state is: **the design is push-preferred, the implementation is
pull-with-an-optimistic-push-attempt.** The receive side has no owner. That is RC-001 and it is
open. Do not read `ok: true` from an inject as proof anything was received — read the target's
own ACK.

This is also why the handoff protocol requires **target-authored** acknowledgement. Text landing
in a pane is not evidence. A receipt written by the receiver is.

## 4. What is actually proven

Rally works, daily, on a small number of fresh macOS installs driven by one operator running
several coding agents.

That is the extent of the evidence. Specifically not proven:

- Linux, beyond CI.
- Hosts other than Claude Code, Codex, Cursor, and Gemini.
- More than one human on the same repo. The trust model assumes one operator and
  [says so](security/TRUST-MODEL.md).
- Any deployment where the ledger crosses a machine boundary. Network transport is explicitly out
  of scope; Rally defines what the bytes mean, not how they move.

Expect edge cases outside that envelope. The repository's executable tests are
the public evidence for current controls; release-session diagnostics and incident
records remain maintainer material.

A root cause that does not survive contact with recurrence was a hypothesis, not a cause.

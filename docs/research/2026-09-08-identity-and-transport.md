<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr | SPDX-License-Identifier: Apache-2.0 -->
# Session identity and host transport investigation

Rally needs separate evidence for stable identity, endpoint registration, input
submission, model execution, receiver acknowledgement, and accepted results.
A successful earlier stage does not prove the next one.

## Verified failure boundaries

| Path | Observation | Boundary |
| --- | --- | --- |
| Cursor and Antigravity editor chats | Both earlier injections returned `commands=[]`, `ledger_only`, `queued_no_managed_session`, `delivered=false`; no ACK within the two-second probe | No synchronous backend was called. The targets were presence-only. This was not a tmux failure. |
| Ordinary exported session ID | Baseline generic `RALLY_SESSION_ID` minted `sess:managed:`; referenced return failed because no runner supported that identity | Identity contract conflicted with ordinary onboarding. |
| Cursor CLI ACP | Local server answered `initialize`; `session/new` returned -32000 / Authentication required | Protocol reachable, authentication blocked. No model turn tested. |
| Antigravity terminal UI | Managed launch bound a live pane, then TUI showed first-run setup and a terms/data-use choice | Live process was not yet an agent input prompt. No work injected into the consent screen. |
| Experimental tmux-to-structured bridge | Trial 1 returned one exact model reply, then timed out; trial 2 timed out after host init | Prototype output handling and startup/result boundaries required investigation. |

Antigravity’s earlier GUI receipt appeared 11m44s after its probe, following
manual UI assistance. It proves queue recovery, not automatic GUI delivery.
Cursor quota and Antigravity provider overload were separate execution failures.
The transcript audit used deterministic digests; native Antigravity transcript
coverage was incomplete. Model reviews are not additional runtime trials.

## What passed

- Stable parent identity now differs from managed-runner identity. The focused
  regression suite checks ordinary referenced returns, exact receiver ACK,
  ledger-only delivery reporting, stale managed refusal, and lifecycle exports
  that clear inherited managed mode for a new parent lease.
- The candidate passed all eight stock-tmux transport/fault checks: frame
  integrity, payload size, concurrent writes, exact pane binding, copy mode,
  failed initial binding, replaced process and unverified-delivery semantics.
  These tests do not involve an LLM.
- A controlled parser simulation showed that correct bytes can still miss
  submission when the host delays processing a paste. One observed pending
  payload recovered with one Enter; blind payload resend duplicated execution.
  The simulated 250ms delay is not an observed Cursor/Antigravity characteristic.
- Antigravity CLI 1.1.27 returned a live nonce and retained a code across two
  structured input turns. The official download matched its manifest SHA-512.
- The bridge’s third trial passed two live turns through Rally, stock tmux and
  Antigravity’s structured CLI. Both exact replies came from the same provider
  conversation. It took 25.703s: first frame-to-result 22.575s, second 2.824s.
  The first host init took 13.891s. This is one successful prototype trial after
  two failed trials, not an SLA, native TUI/GUI success, or a reliability rate.
  Free-text injections did not produce Rally protocol ACKs; host responses are
  separate evidence.

A deterministic pipe experiment reproduced a prototype reader flaw: buffered
`readline()` can consume a later JSON event into user-space while `select()`
reports no new descriptor data. The corrected reader uses a dedicated reader
and event queue. Trial 3 also added timestamps and a longer observation budget;
its success does not isolate the cause of trial 2’s timeout.

## Implementation direction and early gates

Keep stock tmux for registered terminal hosts. Prefer structured protocols when
available: they expose conversation IDs, result events and permission requests
without depending on editor focus or terminal Enter behavior. Keep the Rally
wire protocol independent of the chosen provider and model.

1. Admission must distinguish process alive, host initialized, input ready,
   auth unavailable, permission pending, provider busy and stopped. Never send
   a task into first-run setup or infer readiness from pane existence.
2. Persist one directive ID and immutable payload reference. Record enqueue,
   submission, receiver ACK and accepted result separately, with exact session
   and generation. Unknown outcome triggers lookup, not blind resubmission.
3. Keep one conversation per intended worker context; send task deltas and
   artifact pointers. Bound queued bytes and diagnostics. Measure actual usage
   separately from payload bytes and cache counters.
4. Before shipping an adapter, pass repeated no-human nonce/ACK round trips,
   lost-result and duplicate-send tests, startup/permission delays, wrong-session
   replies, process replacement, busy turns and restart recovery. First falsifier
   is a false success or a duplicate execution; preserve the failed trace.
5. A tmux fork is not justified by these findings. Reconsider only after a
   required terminal primitive fails a reproducible stock-tmux experiment.

## Sources and reproduction

Official interfaces checked on 2026-09-08:
[Cursor ACP](https://cursor.com/docs/cli/acp),
[Cursor CLI parameters](https://cursor.com/docs/cli/reference/parameters),
[Antigravity headless mode](https://www.antigravity.google/docs/cli/headless/),
[tmux control mode](https://github.com/tmux/tmux/wiki/Control-Mode).

Local run evidence is retained under
`.build-loop/data/identity-transport-research-20260908/`: deterministic miner
review, baseline refusal, focused tests, primary-source packet, actual CLI
responses, simulation scripts, all three bridge attempts, model reviews and
quality-gate output. These artifacts are local and not part of the public
package. The scratch bridge is deliberately not installed as a Rally adapter.

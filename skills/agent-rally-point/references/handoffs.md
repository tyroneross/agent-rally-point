## Sending a Handoff Document

When the work produces a handoff **document** rather than a task packet, the document's
location is the payload. A handoff nobody can find has not been sent.

**Post the path, not the prose.** The document already carries the detail; the rally event
carries where to get it.

```bash
rally say fact --tool "$TOOL" \
  --subject "handoff: <one line, what state the work is in>" \
  --evidence "file:<ABSOLUTE path>" --json
```

Absolute, always. A receiver is in a different working directory and often a different
repo, so a relative path resolves to nothing or, worse, to the wrong file.

**Say it in the terminal too.** The human running the session is a receiver as well, and
they are reading a terminal, not the ledger. End the reply with the full path.

**Retrospectives follow the same rule and need it more.** They are written to the memory
store rather than the repo, so nothing in the working directory points at them. Post the
path or it is lost to everyone who was not in the session.

**What this prevents.** The usual failure is not a thin handoff. It is a thorough one that
the next agent never located, so the work is redone from scratch while the document sits
committed three directories away.

## Receiving a Handoff

Everything above is written from the sender's side. This section is the
receiver's, and its first rule is the one that matters most:

**ACK the instant you see a handoff targeting you — before you read the brief,
before you plan, before you touch a file.** The ACK is two seconds of work and
it is the only signal that separates "received" from "never delivered." Reading
a long brief first, then acking, leaves the sender unable to tell which happened.

Use the inbox's generated `ack_command` for bound and unbound handoffs. It
records typed acceptance before work; the first typed response binds a legacy
request to the receiver session. Acceptance keeps assigned work open. Never
invent or override a target binding.

```bash
rally say handoff --tool "$TOOL" --ref <their-event-id> --target <their-tool> \
  --handoff-state acked --subject "ACK — <lane>" --summary "Received. Reading the brief now." --json
```

Then, while you work the handoff, **post a status every ~10 minutes**. Silence
longer than ~15 minutes is a coordination bug: peers cannot distinguish working
from hung.

```bash
rally status post --tool "$TOOL" --state working --intent "<one-line>"
```

### Waiting — pull automatically

Waiting is never passive. Whenever you are waiting on a Rally response — a
handoff, an ACK, a blocker resolution, a decision, an artifact — fetch it
yourself. Do not wait to be woken, and do not ask the user to relay it. Rally has
**no push path**: `rally next` mints the wake intent only when you pull it.

Injection is the primary channel *only* when the target is a managed session
(`rally sessions` lists it). Pulling is always on: it is the fallback when
injection cannot deliver, and the confirmation channel when it can.

Use the shipped watcher rather than hand-rolling a poll loop:

```bash
rally watch --tool "$TOOL" --interval 5 --max-interval 300 \
  --duration-hours 1 --on-activity 'rally next --tool "$TOOL" --json'
```

Pick `--interval` from the cadence of the expected response: 3–5s when the peer
says it is handing off now, 30–60s when you are blocked behind its task, 5–15 min
when it is idle or the job runs overnight. `--max-interval` backs off from there;
`--duration-hours` is the deadline that keeps the watch bounded.

Two things a watcher must get right, whether it is `rally watch` or your own:

- **Baseline first.** Record what already exists on the first pass and report
  only facts newer than that, or you replay room history as if it were new.
- **Every room the peer might be in.** Rooms are cwd-scoped, so watch the repo
  room *and* the room for the cwd the peer's session was launched from. A peer
  posting from `~` is invisible to a watcher scoped to the repo.

### Re-resolve the target in the room you are posting to

A tool id is only meaningful inside one room. An id learned from a SessionStart
prompt, an older log, or another repo may name a session that is stale — or
absent — in the room you are about to post to. Before setting `--target`, read
the live roster **of that room**:

```bash
rally room --json      # squads[]: who is here, last_seen_ts, status, freshness, age_secs
rally next --tool "$TOOL" --json   # next.peer_targets: visible peers ranked freshest-first
rally status read --json
```

Each squad row carries `freshness` (`fresh` = heartbeat inside its adaptive
presence window, `stale` = past it, `unknown` = unparseable timestamp) with
`age_secs` and `window_secs`; the `room` human line tallies `squads=N fresh=X
stale=Y`. `next.peer_targets` ranks every visible peer fresh → unknown → stale,
youngest first, self excluded (`fresh`/`stale`/`unknown` counts cover the whole
room; `ranked` is a shortlist, `truncated` says how many were cut). Prefer a
fresh peer when several could take the work.

A stale peer is still a legal target: `rally say ... --target <stale-peer>`
commits and delivers, and attaches a `stale-target` entry in `warnings[]` naming
the freshest alternatives. Rally advises and ranks; the choice stays with you —
a returning session or a scheduled agent may be exactly who you mean.

Target the session that is actually working the paths in question — an active
claim on the file under discussion is stronger evidence of the right peer than
any id you were handed.

### A room read that looks empty usually is not

`rally next` recommends *one* action and skews toward stale unconsumed
artifacts, so it can return "nothing actionable" while the room holds exactly
what you need. Before concluding a peer has not responded, read the room itself:
active claims name the files a peer is working, and a claim on the brief path is
the handoff arriving one step early.

Treat a cheap negative result as unproven when the method could not have seen
the thing you are looking for.

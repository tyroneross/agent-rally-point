<!--
SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
SPDX-License-Identifier: Apache-2.0
-->

# Maintenance issue list — 2026-05-31

Consolidated from a review of this session's thread + the 5 most recent prior threads
(parallel transcript mine) + a regression check after the `worktree_guard` merge. Fold
the rally-crate rows into `BACKLOG.md` once Codex's in-flight B19 lands (avoids a merge
conflict on the contested crate now). Status keys: ✅ done · ⛔ blocked · ⤳ out-of-scope.

## Fixed this pass

- ✅ **Rally monitor re-fired the same issues every tick** (noise) and flagged *stopped*-agent
  stale claims as forever-actionable. Rewrote `~/.rally-monitor/monitor.py`: delta-based (each
  issue surfaces once via a stable key) + stopped-agent-aware (a stopped agent's stale claim is
  INFO "clears on resume", not MED). Verified: two back-to-back runs exit 0 silent.
- ✅ **`autoUpdate:true` → GC churn** (recurred across prior threads): verified already `false`
  for rosslabs-ai-toolkit + build-loop reinstalled from local HEAD this session. The durable fix
  is in place.

## Open — rally crate (⛔ blocked on Codex B19 / `crates/rally-cli`)

- **Session-registry liveness validation** — `rally sessions` lists stale agents whose tmux pane
  is gone (zombie `ts2-01`). Codex already filed this as a risk. Fix: validate backend target
  liveness / surface `stale` status on the session projection. *(Same detect-and-warn pattern as
  the shipped `worktree_guard`.)*
- **Error-envelope uniformity** — success output is now uniform (`data[command]`), but error
  responses still use a different shape (`{error, exit_code, ok:false}`). Bring errors under the
  same contract so consumers parse one envelope.
- **B18 micro-hardening** — `command_route_findings` / `command_backlog` don't `classify_scope`
  on write (low value; their facts are repo-local or safe risk facts).

## Open — infra / other surfaces (⤳ not this repo)

- **Easy Terminal should launch agents via `rally run`** — enables direct `rally inject`
  (the urgent dual-channel rule) + a real session record (no zombie/never-registered mismatch).
- **Safety-classifier outage** (Anthropic-side) blocked writes ~1.5h across prior threads and hit
  this session too — fails closed. Not locally fixable; noted as recurring infra fragility.
- **build-loop: `build-orchestrator` never dispatches the independent-auditor when run as a
  subagent** (no sub-sub-agents) — Phase 4-A audit silently skips. Fix: write the audit brief to
  rally and let the orchestrating session dispatch it. *(build-loop repo; concurrent session.)*
- **No automated `plugin.json` version-sync check** across RossLabs-AI-Toolkit (catalog drifts
  behind repos). *(toolkit; concurrent session.)*
- **codex:rescue** failures are environment/dispatch (sandbox flags, auth, cwd), not reasoning —
  a few dispatch-brief fixes recapture most utility. *(openai-codex / external.)*

## Pruned (no longer relevant)

- Earlier "Claude not in rally / needs a hook" — superseded: the Rust binary's lazy-auto-enter is
  hookless; the bespoke hook was the legacy pattern.
- "build-loop plugin undispatchable this session" — mitigated by the local-HEAD reinstall.

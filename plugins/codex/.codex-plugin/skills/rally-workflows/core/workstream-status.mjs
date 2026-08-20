#!/usr/bin/env node
// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
// workstream-status — the DURABLE counterpart to pi-dynamic-workflows' in-memory
// RuntimeState.agentCount. Pi tracks progress in a parent process's RAM, so a crash
// loses everything and there is no resume. Here, progress lives in Rally facts, so a
// fresh agent / new session / different host can reconstruct exactly which tasks are
// done and re-dispatch only the rest. This is the long-running improvement over pi.
//
// It only READS and DERIVES (charter: Rally never executes). It tells the host what
// remains; the host spawns the work.

/**
 * Given a workstream descriptor and a `rally room --json` snapshot, classify each
 * task as done | claimed | active | pending and compute the resume set (pending tasks whose
 * dependencies are all done).
 *
 * Convention (documented in PROTOCOL.md "Durable fan-out & resume"):
 *   - a task is DONE when a rally artifact's subject names the task id
 *     (recommended: `rally say artifact --subject "<task.id>: <result>" ...`)
 *   - a task is CLAIMED when an active claim names the task id or overlaps its `owns`
 *   - a READ-ONLY task is ACTIVE when the room's transitional squad projection has
 *     the exact active task tool `<tool-prefix>:<task-id>`
 *
 * Usage:  node workstream-status.mjs <descriptor.json> <rally-room.json|-> [--tool-prefix <prefix>]
 * Exit:   0 = complete (nothing left to dispatch) · 3 = work remains · 2 = usage/parse error
 */

import { readFileSync } from "node:fs";

/**
 * Pull the room object out of whatever shape `rally room --json` (or a raw room) gives.
 * - `raw.data.room`: the live CLI envelope shape. Every rally command now follows one
 *   contract — `{ ok, command, product, schema, data }` with the result at `data[<command>]`,
 *   so `rally room --json` puts the room at `raw.data.room` (standardized 2026-05-31;
 *   see ../docs/JSON_ENVELOPE.md). This is the canonical path.
 * - `raw.room`: a thinner `{ room }` wrapper some callers pass.
 * - else: `raw` is already a bare room object (tests + in-process callers).
 */
export function extractRoom(raw) {
  if (raw && raw.data && raw.data.room) return raw.data.room;
  if (raw && raw.room) return raw.room;
  return raw || {};
}

/** Whole-token match: does `subject` reference `id` as a discrete token? */
function mentions(subject, id) {
  if (typeof subject !== "string" || !id) return false;
  // word-ish boundary: id surrounded by start/end or a non-[A-Za-z0-9_-] char
  const tokens = subject.split(/[^A-Za-z0-9_.:-]+/);
  if (tokens.includes(id)) return true;
  // also accept "id:" prefix form
  return subject.startsWith(`${id}:`) || subject.startsWith(`${id} `);
}

function scopesOverlap(scope, owns) {
  if (!Array.isArray(scope) || owns === "read-only" || !Array.isArray(owns)) return false;
  // norm() here strips a leading `file:` scheme (rally claims carry `file:`-prefixed scopes)
  // and trailing slashes, for EXACT-MATCH comparison between a claim's scope and a task's owns.
  // It deliberately does NOT strip trailing globs — this is exact-match claim detection, not the
  // lint's prefix-overlap MECE check. The two norm() helpers are intentionally different; do not merge.
  const norm = (p) => String(p).replace(/^file:/, "").replace(/\/+$/, "");
  return scope.some((s) => owns.some((o) => norm(s) === norm(o)));
}

export function workstreamStatus(descriptor, room, { toolPrefix = "agent" } = {}) {
  if (!descriptor || !Array.isArray(descriptor.tasks)) {
    throw new Error("descriptor must have a tasks array");
  }
  if (typeof toolPrefix !== "string" || !/^[A-Za-z0-9._-]+$/.test(toolPrefix)) {
    throw new Error("toolPrefix must match /^[A-Za-z0-9._-]+$/");
  }
  const artifacts = Array.isArray(room.recent_artifacts) ? room.recent_artifacts : [];
  const claims = Array.isArray(room.active_claims) ? room.active_claims : [];
  const squads = Array.isArray(room.squads) ? room.squads : [];

  // Task ids may legally be `__proto__`, so a normal object would invoke its
  // inherited setter instead of recording that task and could false-complete.
  const status = Object.create(null); // id -> 'done' | 'claimed' | 'active' | 'pending'
  for (const t of descriptor.tasks) {
    if (!t || !t.id) continue;
    const done = artifacts.some((a) => mentions(a.subject, t.id));
    if (done) {
      status[t.id] = "done";
      continue;
    }
    const readOnly = t.owns === "read-only";
    const claimed = !readOnly && claims.some((c) => mentions(c.subject, t.id) || scopesOverlap(c.scope, t.owns));
    if (claimed) {
      status[t.id] = "claimed";
      continue;
    }
    // Transitional O33-B heuristic: default room snapshots do not expose the
    // run/step-scoped presence fact. Until O33-C adds active_activities, only an
    // exact, fresh task-tool identity can hold a read-only task in-flight.
    const taskTool = `${toolPrefix}:${t.id}`;
    const active = readOnly && squads.some((s) => s?.tool === taskTool && s?.status === "active");
    status[t.id] = active ? "active" : "pending";
  }

  const isDone = (id) => status[id] === "done";
  const toDispatch = descriptor.tasks
    .filter((t) => t && t.id && status[t.id] === "pending")
    .filter((t) => (Array.isArray(t.depends_on) ? t.depends_on : []).every(isDone))
    .map((t) => t.id);

  const ids = descriptor.tasks.filter((t) => t && t.id).map((t) => t.id);
  const done = ids.filter((id) => status[id] === "done");
  const claimedList = ids.filter((id) => status[id] === "claimed");
  const active = ids.filter((id) => status[id] === "active");
  const pending = ids.filter((id) => status[id] === "pending");

  return {
    workstream: descriptor.workstream ?? null,
    thread: descriptor.thread ?? null,
    total: ids.length,
    done,
    claimed: claimedList,
    active,
    pending,
    to_dispatch: toDispatch, // pending AND deps satisfied → resume here
    complete: pending.length === 0 && claimedList.length === 0 && active.length === 0,
    status,
  };
}

// ---- CLI ----
function main(argv) {
  const [descPath, roomPath] = [argv[2], argv[3]];
  if (!descPath || !roomPath) {
    process.stderr.write("usage: node workstream-status.mjs <descriptor.json> <rally-room.json|-> [--tool-prefix <prefix>]\n");
    return 2;
  }
  let toolPrefix = "agent";
  const rest = argv.slice(4);
  if (rest.length > 0) {
    if (rest.length !== 2 || rest[0] !== "--tool-prefix") {
      process.stderr.write(`unexpected arguments: ${rest.join(" ")}\n`);
      return 2;
    }
    toolPrefix = rest[1];
  }
  let descriptor, room;
  try {
    descriptor = JSON.parse(readFileSync(descPath, "utf8"));
  } catch (e) {
    process.stderr.write(`cannot read descriptor ${descPath}: ${e.message}\n`);
    return 2;
  }
  try {
    const roomRaw = roomPath === "-" ? readFileSync(0, "utf8") : readFileSync(roomPath, "utf8");
    room = extractRoom(JSON.parse(roomRaw));
  } catch (e) {
    process.stderr.write(`cannot read rally room ${roomPath}: ${e.message}\n`);
    return 2;
  }
  let out;
  try {
    out = workstreamStatus(descriptor, room, { toolPrefix });
  } catch (e) {
    process.stderr.write(`${e.message}\n`);
    return 2;
  }
  process.stdout.write(JSON.stringify(out, null, 2) + "\n");
  if (out.complete) {
    process.stderr.write(`✓ workstream complete (${out.done.length}/${out.total} done)\n`);
    return 0;
  }
  process.stderr.write(
    `… ${out.done.length}/${out.total} done · ${out.claimed.length} claimed · ${out.active.length} read-active · ${out.pending.length} pending · dispatch now: [${out.to_dispatch.join(", ")}]\n`,
  );
  return 3; // work remains — host re-dispatches the to_dispatch set
}

if (import.meta.url === `file://${process.argv[1]}`) {
  process.exit(main(process.argv));
}

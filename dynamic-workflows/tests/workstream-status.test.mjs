// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

import { test } from "node:test";
import assert from "node:assert/strict";
import { workstreamStatus, extractRoom } from "../core/workstream-status.mjs";

const descriptor = {
  workstream: "demo",
  description: "d",
  thread: "ws_demo",
  tasks: [
    { id: "a", intent: "i", owns: ["src/a"], validation: "v", output: "o" },
    { id: "b", intent: "i", owns: ["src/b"], validation: "v", output: "o", depends_on: ["a"] },
    { id: "c", intent: "i", owns: "read-only", validation: "v", output: "o", depends_on: ["a", "b"] },
  ],
};

// rally room shape: artifacts mark done; active_claims mark in-flight
const room = (artifactSubjects = [], claims = []) => ({
  recent_artifacts: artifactSubjects.map((s) => ({ subject: s })),
  active_claims: claims,
});

test("fresh workstream: only dep-free tasks are dispatchable", () => {
  const s = workstreamStatus(descriptor, room());
  assert.deepEqual(s.done, []);
  assert.deepEqual(s.to_dispatch, ["a"]); // b,c blocked by deps
  assert.equal(s.complete, false);
});

test("resume: a done → b becomes dispatchable, c still blocked", () => {
  const s = workstreamStatus(descriptor, room(["a: implemented and verified"]));
  assert.deepEqual(s.done, ["a"]);
  assert.deepEqual(s.to_dispatch, ["b"]);
  assert.ok(s.pending.includes("c"));
});

test("in-flight claim marks a task claimed, not dispatchable", () => {
  const s = workstreamStatus(descriptor, room([], [{ subject: "working on a", scope: ["file:src/a"] }]));
  assert.deepEqual(s.claimed, ["a"]);
  assert.deepEqual(s.to_dispatch, []); // a is claimed, not pending
});

test("all done → complete", () => {
  const s = workstreamStatus(descriptor, room(["a: done", "b: done", "c: reviewed"]));
  assert.equal(s.complete, true);
  assert.deepEqual(s.pending, []);
  assert.deepEqual(s.to_dispatch, []);
});

test("done-detection is whole-token (does not match a substring of another id)", () => {
  const d2 = { workstream: "w", description: "d", tasks: [
    { id: "build", intent: "i", owns: "read-only", validation: "v", output: "o" },
    { id: "rebuild", intent: "i", owns: "read-only", validation: "v", output: "o" },
  ] };
  // an artifact for "rebuild" must NOT mark "build" done
  const s = workstreamStatus(d2, room(["rebuild: done"]));
  assert.deepEqual(s.done, ["rebuild"]);
  assert.ok(s.pending.includes("build"));
});

test("extractRoom unwraps the live `rally room --json` envelope (raw.data.room)", () => {
  // The actual CLI shape (verified 2026-05-29): { command, data: { query, room }, ok, ... }
  const envelope = { command: "room", ok: true, data: { query: {}, room: { recent_artifacts: [{ subject: "x" }], active_claims: [] } } };
  const r = extractRoom(envelope);
  assert.deepEqual(r, { recent_artifacts: [{ subject: "x" }], active_claims: [] });
});

test("extractRoom unwraps a thin { room } wrapper and passes a bare room through", () => {
  assert.deepEqual(extractRoom({ room: { active_claims: [] } }), { active_claims: [] });
  const bare = { recent_artifacts: [{ subject: "a: done" }], active_claims: [] };
  assert.deepEqual(extractRoom(bare), bare);
  assert.deepEqual(extractRoom(null), {});
});

test("workstreamStatus accepts a bare room object", () => {
  const s = workstreamStatus(descriptor, { recent_artifacts: [{ subject: "a: done" }], active_claims: [] });
  assert.deepEqual(s.done, ["a"]);
});

// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { workstreamStatus, extractRoom } from "../core/workstream-status.mjs";

const STATUS_CLI = fileURLToPath(new URL("../core/workstream-status.mjs", import.meta.url));

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

// Transitional rally room shape: artifacts mark done, active_claims mark write
// ownership, and an exact active task-tool squad marks nonexclusive read work.
const room = (artifactSubjects = [], claims = [], squads = []) => ({
  recent_artifacts: artifactSubjects.map((s) => ({ subject: s })),
  active_claims: claims,
  squads,
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

test("read_only_active_exact_task_tool_is_nonexclusive_and_not_redispatched", () => {
  const s = workstreamStatus(
    descriptor,
    room(
      ["a: done", "b: done"],
      [],
      [{ tool: "reviewer:c", status: "active" }],
    ),
    { toolPrefix: "reviewer" },
  );
  assert.deepEqual(s.claimed, [], "read activity must not be reported as ownership");
  assert.deepEqual(s.active, ["c"]);
  assert.deepEqual(s.to_dispatch, []);
  assert.equal(s.status.c, "active");
  assert.equal(s.complete, false);
});

test("read-only resume heuristic requires the exact active task tool", () => {
  const base = ["a: done", "b: done"];
  for (const squad of [
    { tool: "reviewer:cc", status: "active" },
    { tool: "other:c", status: "active" },
    { tool: "reviewer:c", status: "idle" },
  ]) {
    const s = workstreamStatus(descriptor, room(base, [], [squad]), { toolPrefix: "reviewer" });
    assert.deepEqual(s.active, [], `must not accept ${JSON.stringify(squad)}`);
    assert.deepEqual(s.to_dispatch, ["c"]);
  }
});

test("legacy read-only claims do not restore exclusive ownership semantics", () => {
  const s = workstreamStatus(
    descriptor,
    room(
      ["a: done", "b: done"],
      [{ subject: "c", scope: [] }],
    ),
    { toolPrefix: "reviewer" },
  );
  assert.deepEqual(s.claimed, []);
  assert.deepEqual(s.active, []);
  assert.deepEqual(s.to_dispatch, ["c"]);
});

test("write-task presence without a claim does not suppress dispatch", () => {
  const s = workstreamStatus(
    descriptor,
    room([], [], [{ tool: "agent:a", status: "active" }]),
    { toolPrefix: "agent" },
  );
  assert.deepEqual(s.claimed, []);
  assert.deepEqual(s.active, []);
  assert.deepEqual(s.to_dispatch, ["a"]);
});

test("reserved_object_keys_do_not_false_complete_resume", () => {
  const reserved = {
    workstream: "reserved-ids",
    tasks: ["__proto__", "constructor", "prototype"].map((id) => ({
      id,
      owns: "read-only",
    })),
  };
  const s = workstreamStatus(reserved, room());
  assert.equal(s.complete, false);
  assert.deepEqual(s.pending, ["__proto__", "constructor", "prototype"]);
  assert.deepEqual(s.to_dispatch, ["__proto__", "constructor", "prototype"]);
  for (const id of s.pending) {
    assert.equal(s.status[id], "pending");
    assert.ok(Object.hasOwn(s.status, id), `${id} must be an own task-state key`);
  }
});

test("CLI applies the same explicit tool prefix to read-only resume", () => {
  const dir = mkdtempSync(join(tmpdir(), "rally-workstream-status-"));
  try {
    const descriptorPath = join(dir, "workstream.json");
    const roomPath = join(dir, "room.json");
    writeFileSync(descriptorPath, JSON.stringify(descriptor));
    writeFileSync(roomPath, JSON.stringify(room(
      ["a: done", "b: done"],
      [],
      [{ tool: "reviewer:c", status: "active" }],
    )));
    const result = spawnSync(
      process.execPath,
      [STATUS_CLI, descriptorPath, roomPath, "--tool-prefix", "reviewer"],
      { encoding: "utf8" },
    );
    assert.equal(result.status, 3, result.stderr);
    const status = JSON.parse(result.stdout);
    assert.deepEqual(status.active, ["c"]);
    assert.deepEqual(status.claimed, []);
    assert.deepEqual(status.to_dispatch, []);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
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

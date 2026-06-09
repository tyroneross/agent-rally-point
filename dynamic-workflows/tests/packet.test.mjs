// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  renderPacket,
  renderAll,
  toolIdFor,
  parseArgs,
} from "../core/packet.mjs";

// A linted-clean 2-task fixture: one read-write root, one dependent write-task.
const FIXTURE = {
  workstream: "two-task fan-out fixture",
  description: "drop-in context for the round-trip test",
  tasks: [
    {
      id: "t1",
      intent: "extract the foo helper",
      owns: ["src/foo.js"],
      validation: "node --check src/foo.js && echo OK",
      output: "src/foo.js exporting a pure foo()",
    },
    {
      id: "t2",
      intent: "wire foo into the entrypoint",
      owns: ["src/index.js"],
      validation: "node --check src/index.js && echo OK",
      output: "src/index.js importing foo",
      depends_on: ["t1"],
    },
  ],
};

const RUN = "run-fixture-001";

test("packet embeds the task's owns paths", () => {
  const p = renderPacket({ task: FIXTURE.tasks[0], runId: RUN, toolPrefix: "agent" });
  assert.ok(p.includes("src/foo.js"), "expected owns path in packet body");
  assert.ok(p.includes("--path src/foo.js"), "expected owns path on the rally claim line");
});

test("packet embeds --run/--step markers for the task", () => {
  const p = renderPacket({ task: FIXTURE.tasks[0], runId: RUN, toolPrefix: "agent" });
  assert.ok(p.includes(`--run ${RUN}`), "expected --run marker");
  assert.ok(p.includes("--step t1"), "expected --step marker");
});

test("dependent task carries a --parent-step marker", () => {
  const p = renderPacket({ task: FIXTURE.tasks[1], runId: RUN, toolPrefix: "agent" });
  assert.ok(p.includes("--parent-step t1"), "expected --parent-step for depends_on");
  assert.ok(p.includes("--step t2"), "expected its own --step marker");
});

test("packet embeds validation, output contract, and final-JSON discipline", () => {
  const p = renderPacket({ task: FIXTURE.tasks[0], runId: RUN, toolPrefix: "agent" });
  assert.ok(p.includes("node --check src/foo.js && echo OK"), "expected validation command");
  assert.ok(p.includes("src/foo.js exporting a pure foo()"), "expected output contract");
  assert.ok(p.includes('"validation_result"'), "expected the single-JSON result discipline");
  assert.ok(/no prose after/i.test(p), "expected the no-prose-after rule");
});

test("tool-prefix derives <prefix>:<task-id>", () => {
  assert.equal(toolIdFor("t1", "claude_code"), "claude_code:t1");
  const p = renderPacket({ task: FIXTURE.tasks[0], runId: RUN, toolPrefix: "claude_code" });
  assert.ok(p.includes("--tool claude_code:t1"), "expected derived tool id on rally commands");
});

test("renderAll round-trips a 2-task fixture (one packet per task)", () => {
  const packets = renderAll({ doc: FIXTURE, runId: RUN, toolPrefix: "agent" });
  assert.equal(packets.length, 2, "expected one packet per task");
  assert.deepEqual(packets.map((p) => p.id), ["t1", "t2"]);
  assert.deepEqual(packets.map((p) => p.tool), ["agent:t1", "agent:t2"]);
  // each packet is self-contained for its own task
  assert.ok(packets[0].content.includes("--step t1"));
  assert.ok(packets[1].content.includes("--step t2"));
});

test("renderAll filters to a single named task", () => {
  const packets = renderAll({ doc: FIXTURE, runId: RUN, task: "t2", toolPrefix: "agent" });
  assert.equal(packets.length, 1);
  assert.equal(packets[0].id, "t2");
});

test("renderAll rejects an unknown --task", () => {
  assert.throws(
    () => renderAll({ doc: FIXTURE, runId: RUN, task: "ghost", toolPrefix: "agent" }),
    /not found/,
  );
});

test("renderAll refuses a descriptor that fails the linter", () => {
  const bad = {
    workstream: "bad",
    description: "overlapping owns",
    tasks: [
      { id: "a", intent: "i", owns: ["src"], validation: "v", output: "o" },
      { id: "b", intent: "i", owns: ["src/foo.js"], validation: "v", output: "o" },
    ],
  };
  assert.throws(() => renderAll({ doc: bad, runId: RUN, toolPrefix: "agent" }), /workstream-lint/);
});

test("read-only task renders no before-write check and a placeholder uri", () => {
  const doc = {
    workstream: "ro",
    description: "read-only review",
    tasks: [{ id: "r1", intent: "review", owns: "read-only", validation: "true", output: "notes" }],
  };
  const p = renderPacket({ task: doc.tasks[0], runId: RUN, toolPrefix: "agent" });
  assert.ok(/read-only/i.test(p), "expected read-only language");
  assert.ok(!p.includes("before-write --tool agent:r1 --path"), "read-only task claims no path");
});

test("determinism: identical inputs yield byte-identical packets", () => {
  const a = renderPacket({ task: FIXTURE.tasks[0], runId: RUN, toolPrefix: "agent" });
  const b = renderPacket({ task: FIXTURE.tasks[0], runId: RUN, toolPrefix: "agent" });
  assert.equal(a, b, "packets must be reproducible across hosts");
});

test("parseArgs requires a file and --run", () => {
  assert.throws(() => parseArgs(["node", "packet.mjs"]), /missing <descriptor.json>/);
  assert.throws(() => parseArgs(["node", "packet.mjs", "d.json"]), /--run <run_id> is required/);
  const ok = parseArgs(["node", "packet.mjs", "d.json", "--run", "r", "--task", "t1", "--tool-prefix", "x"]);
  assert.deepEqual(ok, { file: "d.json", runId: "r", task: "t1", out: null, toolPrefix: "x" });
});

// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { lintWorkstream } from "../core/workstream-lint.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const load = (name) => JSON.parse(readFileSync(join(here, "..", "examples", name), "utf8"));

test("accepts the valid example descriptor", () => {
  const errors = lintWorkstream(load("audit-repo.workstream.json"));
  assert.deepEqual(errors, [], `expected no errors, got: ${errors.join("; ")}`);
});

test("rejects a non-deterministic validation command", () => {
  const errors = lintWorkstream(load("bad-nondeterministic.workstream.json"));
  assert.ok(errors.length > 0, "expected at least one error");
  assert.ok(errors.some((e) => /non-deterministic/i.test(e)), `expected a determinism error, got: ${errors.join("; ")}`);
});

test("rejects a packet missing owns/validation/output", () => {
  const errors = lintWorkstream(load("bad-missing-fields.workstream.json"));
  assert.ok(errors.some((e) => /validation/.test(e)), "expected a missing-validation error");
  assert.ok(errors.some((e) => /output/.test(e)), "expected a missing-output error");
});

test("rejects overlapping owns (MECE boundary conflict)", () => {
  const errors = lintWorkstream(load("bad-missing-fields.workstream.json"));
  assert.ok(errors.some((e) => /boundary conflict/.test(e)), `expected a boundary conflict, got: ${errors.join("; ")}`);
});

test("rejects unknown depends_on and dependency cycles", () => {
  const unknown = lintWorkstream({
    workstream: "w", description: "d",
    tasks: [{ id: "x", intent: "i", owns: "read-only", validation: "v", output: "o", depends_on: ["ghost"] }],
  });
  assert.ok(unknown.some((e) => /unknown task id/.test(e)));

  const cyclic = lintWorkstream({
    workstream: "w", description: "d",
    tasks: [
      { id: "a", intent: "i", owns: "read-only", validation: "v", output: "o", depends_on: ["b"] },
      { id: "b", intent: "i", owns: "read-only", validation: "v", output: "o", depends_on: ["a"] },
    ],
  });
  assert.ok(cyclic.some((e) => /cycle/.test(e)), `expected a cycle error, got: ${cyclic.join("; ")}`);
});

test("rejects a non-object descriptor and empty tasks", () => {
  assert.ok(lintWorkstream(null).length > 0);
  assert.ok(lintWorkstream({ workstream: "w", description: "d", tasks: [] }).some((e) => /non-empty array/.test(e)));
});

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

// f4: the emitted bash interpolates intent inside a double-quoted --subject and
// owns paths bare. Reject the characters that would break that quoting so a
// descriptor can never generate shell-unsafe rally commands.
for (const ch of ['"', "$", "`"]) {
  test(`rejects intent containing ${JSON.stringify(ch)} (breaks emitted --subject quoting)`, () => {
    const errors = lintWorkstream({
      workstream: "w",
      description: "d",
      tasks: [{ id: "x", intent: `do ${ch} thing`, owns: ["src/x.js"], validation: "v", output: "o" }],
    });
    assert.ok(
      errors.some((e) => /intent.*must not contain/.test(e)),
      `expected an intent-quoting error for ${JSON.stringify(ch)}, got: ${errors.join("; ")}`,
    );
  });
}

test("rejects an owns path containing whitespace (would split into multiple --path tokens)", () => {
  const errors = lintWorkstream({
    workstream: "w",
    description: "d",
    tasks: [{ id: "x", intent: "i", owns: ["src/my file.js"], validation: "v", output: "o" }],
  });
  assert.ok(
    errors.some((e) => /owns.*must not contain whitespace/.test(e)),
    `expected an owns-whitespace error, got: ${errors.join("; ")}`,
  );
});

test("accepts a clean intent + owns (no f4 false positives)", () => {
  const errors = lintWorkstream({
    workstream: "w",
    description: "d",
    tasks: [{ id: "x", intent: "extract the foo helper (pure)", owns: ["src/foo-bar.js"], validation: "v", output: "o" }],
  });
  assert.deepEqual(errors, [], `expected no errors, got: ${errors.join("; ")}`);
});

// f1 (auditor): `output` is interpolated into the same double-quoted bash
// --subject as `intent` (the `rally say artifact` line), so it gets the same
// shell-safety rejection. `owns` paths are emitted bare, so they reject quoting
// chars too. `id` is interpolated bare into --step/--parent-step + filenames.
for (const ch of ['"', "$", "`"]) {
  test(`rejects output containing ${JSON.stringify(ch)} (breaks emitted --subject quoting)`, () => {
    const errors = lintWorkstream({
      workstream: "w",
      description: "d",
      tasks: [{ id: "x", intent: "i", owns: ["src/x.js"], validation: "v", output: `result ${ch} here` }],
    });
    assert.ok(
      errors.some((e) => /output.*must not contain/.test(e)),
      `expected an output-quoting error for ${JSON.stringify(ch)}, got: ${errors.join("; ")}`,
    );
  });
}

test("rejects an owns path containing a shell metacharacter (\" $ backtick)", () => {
  for (const bad of ['src/$x.js', 'src/`x`.js', 'src/"x".js']) {
    const errors = lintWorkstream({
      workstream: "w",
      description: "d",
      tasks: [{ id: "x", intent: "i", owns: [bad], validation: "v", output: "o" }],
    });
    assert.ok(
      errors.some((e) => /owns.*must not contain/.test(e)),
      `expected an owns shell-metachar error for ${JSON.stringify(bad)}, got: ${errors.join("; ")}`,
    );
  }
});

test("rejects a task id with a shell/path-unsafe character", () => {
  for (const bad of ["a b", "a/b", "a$b", 'a"b', "a;b"]) {
    const errors = lintWorkstream({
      workstream: "w",
      description: "d",
      tasks: [{ id: bad, intent: "i", owns: ["src/x.js"], validation: "v", output: "o" }],
    });
    assert.ok(
      errors.some((e) => /must match/.test(e)),
      `expected an id-charset error for ${JSON.stringify(bad)}, got: ${errors.join("; ")}`,
    );
  }
});

// No-false-positive guard: the real production descriptor (10 persona tasks with
// JSON-shaped `output` strings, p01..p10 ids, councils/runs/* owns paths) must
// still lint clean under the tightened output/owns/id rules.
test("accepts the production haiku-scale descriptor (no f1 false positives)", () => {
  const prod = "/Users/tyroneross/dev/git-folder/AI User Personas/councils/runs/haiku-scale-20260609-01/workstream.json";
  let doc;
  try {
    doc = JSON.parse(readFileSync(prod, "utf8"));
  } catch {
    // The descriptor lives outside this repo; skip cleanly if absent on this host.
    return;
  }
  const errors = lintWorkstream(doc);
  assert.deepEqual(errors, [], `production descriptor must lint clean, got: ${errors.join("; ")}`);
});

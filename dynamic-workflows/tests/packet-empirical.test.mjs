// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
// Empirical flag-arity gate for packet.mjs.
//
// The unit tests in packet.test.mjs assert the PRESENCE of rally markers in the
// emitted text — which is exactly how f1 (repeated --path on `check before-write`)
// and f2 (repeated --parent-step on `rally say`) escaped review: a marker can be
// present and still be rejected by the real CLI for flag arity.
//
// This gate closes that hole. It generates a packet from a 2-path/2-dep descriptor,
// extracts the emitted `rally claim` + `rally check before-write` lines, and runs
// them against the BUILT release binary in a throwaway temp-dir room. If a future
// edit to packet.mjs (or the CLI's flag arity) drifts, these commands fail and so
// does the test.
//
// Determinism + self-containment: the room is a fresh mktemp dir, git-init'd, torn
// down after. No network, no shared state, no wall-clock in assertions. If the
// release binary is absent (e.g. a JS-only CI lane that never ran `cargo build`),
// the gate is skipped with a clear reason rather than failing spuriously — build
// the binary to arm it.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { renderPacket } from "../core/packet.mjs";

const here = dirname(fileURLToPath(import.meta.url));
// dynamic-workflows/tests -> repo root -> target/release/rally
// NOTE: this binary must be REBUILT after any rally-cli change for this gate to
// test current behavior — a stale binary would assert against old flag arity.
// CI guarantees freshness: .github/workflows/rally-gate.yml runs
// `cargo build --release -p rally-cli` before the node test suite, so the gate
// is always ARMED (not skipped) and always tests the just-built CLI.
const RALLY = join(here, "..", "..", "target", "release", "rally");

const DESCRIPTOR = {
  workstream: "empirical 2-path/2-dep gate",
  description: "executes emitted rally commands against the real binary",
  tasks: [
    { id: "a", intent: "build a", owns: ["src/a.js"], validation: "true", output: "a" },
    { id: "b", intent: "build b", owns: ["src/b.js"], validation: "true", output: "b" },
    {
      id: "c",
      intent: "wire a and b",
      owns: ["src/c.js", "src/c.test.js"],
      validation: "true",
      output: "c",
      depends_on: ["a", "b"],
    },
  ],
};
const RUN = "run-empirical-001";

/** Split a `rally ...` line into argv (the emitted lines use plain space-delimited,
 *  shell-safe tokens — the lint guarantees no whitespace/quote chars get this far). */
function railsToArgv(line) {
  // Drop a trailing line-continuation backslash if present.
  const clean = line.replace(/\\\s*$/, "").trim();
  // Tokenize, honoring double-quoted --subject "..." spans.
  const out = [];
  const re = /"([^"]*)"|(\S+)/g;
  let m;
  while ((m = re.exec(clean)) !== null) out.push(m[1] !== undefined ? m[1] : m[2]);
  return out.slice(1); // drop the leading "rally"
}

const armed = existsSync(RALLY);

test("empirical: emitted claim + before-write lines execute against the real binary (f1/f2 arity)", { skip: armed ? false : `release binary not built at ${RALLY} — run \`cargo build --release -p rally-cli\` to arm` }, () => {
  const room = mkdtempSync(join(tmpdir(), "rally-packet-empirical-"));
  try {
    execFileSync("git", ["init", "-q", room], { stdio: "ignore" });
    const run = (argv) =>
      execFileSync(RALLY, argv, { cwd: room, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });

    // The multi-owns/multi-dep task is the one f1+f2 regressed.
    const p = renderPacket({ task: DESCRIPTOR.tasks[2], runId: RUN, toolPrefix: "agent" });
    const tool = "agent:c";

    run(["enter", "--tool", tool]);

    // Multi-arg claim line — exercises repeated --path AND repeated --parent-step in one go.
    // The claim continues onto the next line (the markers). Strip the trailing
    // line-continuation backslash, then stitch the two-line span into one command.
    const lines = p.split("\n");
    const claimIdx = lines.findIndex((l) => l.startsWith("rally say claim"));
    const claimLine = lines[claimIdx].replace(/\\\s*$/, "").trim();
    const claimFull = `${claimLine} ${lines[claimIdx + 1].trim()}`;
    assert.doesNotThrow(
      () => run(railsToArgv(claimFull)),
      "the emitted multi-path/multi-parent-step claim must execute (f1 claim --path + f2 --parent-step)",
    );
    assert.ok(claimLine.includes("--path"), "sanity: claim line carries owns paths");

    // Each before-write line must execute on its own (f1: one --path per line).
    const checks = p.split("\n").filter((l) => l.startsWith("rally check before-write"));
    assert.equal(checks.length, 2, "expected one before-write per owned path");
    for (const line of checks) {
      assert.doesNotThrow(
        () => run(railsToArgv(line)),
        `before-write line must execute against the real CLI: ${line}`,
      );
    }

    // Ground truth: the DAG must show two parent_step edges into step c.
    const dagJson = run(["dag", "--run", RUN, "--json"]);
    const dag = JSON.parse(dagJson).data.dag;
    const edgesIntoC = dag.edges.filter((e) => e.kind === "parent_step" && e.to_step === "c");
    assert.equal(edgesIntoC.length, 2, `expected 2 parent_step edges into c; got: ${JSON.stringify(edgesIntoC)}`);
    assert.deepEqual(
      edgesIntoC.map((e) => e.from_step).sort(),
      ["a", "b"],
      "both a and b must edge into c",
    );
  } finally {
    rmSync(room, { recursive: true, force: true });
  }
});

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
// release binary is absent, the gate is skipped with a clear reason rather than
// failing spuriously — build the binary to arm it. As of 2026-08-10 the repo's
// CI gate does not invoke this Node suite; the combined O33 activation gate must
// add a current release build plus a zero-skip npm test before rollout.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { renderPacket } from "../core/packet.mjs";
import { workstreamStatus } from "../core/workstream-status.mjs";

const here = dirname(fileURLToPath(import.meta.url));
// dynamic-workflows/tests -> repo root -> target/release/rally
// NOTE: this binary must be REBUILT after any rally-cli change for this gate to
// test current behavior — a stale binary would assert against old flag arity.
// Local/manual evidence must name that build and report zero skipped tests; CI
// does not currently guarantee either property.
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

const READ_ONLY_TASK = {
  id: "review",
  intent: "review the implementation",
  owns: "read-only",
  validation: "true",
  output: "review notes",
};

/** Split a `rally ...` line into argv.
 *
 *  Safety comes from packet.mjs's shellQuote(), NOT from the lint. The lint
 *  constrains identifiers; the renderer quotes every emitted value. So this
 *  tokenizer must understand POSIX single-quoted spans ('...' with '\'' escapes)
 *  as well as double-quoted spans and bare tokens.
 *
 *  It used to honor double quotes only. When --subject moved to single-quoting
 *  (ARP-002), that stale assumption split `'wire a and b'` into four tokens and
 *  fed stray positionals to the real CLI. The emitted command was correct; the
 *  tokenizer was not. */
function railsToArgv(line) {
  // Drop a trailing line-continuation backslash if present.
  const clean = line.replace(/\\\s*$/, "").trim();
  const out = [];
  const re = /'((?:[^']|'\\'')*)'|"([^"]*)"|(\S+)/g;
  let m;
  while ((m = re.exec(clean)) !== null) {
    if (m[1] !== undefined) out.push(m[1].replaceAll("'\\''", "'"));
    else if (m[2] !== undefined) out.push(m[2]);
    else out.push(m[3]);
  }
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

test("read_only_packet_runtime_creates_zero_active_claims", { skip: armed ? false : `release binary not built at ${RALLY} — run \`cargo build --release -p rally-cli\` to arm` }, () => {
  const room = mkdtempSync(join(tmpdir(), "rally-packet-read-only-"));
  try {
    execFileSync("git", ["init", "-q", room], { stdio: "ignore" });
    const run = (argv) =>
      execFileSync(RALLY, argv, { cwd: room, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
    const packet = renderPacket({ task: READ_ONLY_TASK, runId: RUN, toolPrefix: "agent" });
    const lines = packet.split("\n");
    const tool = "agent:review";

    run(["enter", "--tool", tool]);

    const activityIdx = lines.findIndex((line) => line.startsWith("rally say presence"));
    assert.notEqual(activityIdx, -1, "packet must emit a nonexclusive activity command");
    const activityFull = `${lines[activityIdx].replace(/\\\s*$/, "").trim()} ${lines[activityIdx + 1].trim()}`;
    const activity = JSON.parse(run([...railsToArgv(activityFull), "--json"])).data.say.fact;
    assert.equal(activity.kind, "presence");
    assert.equal(activity.status, "working");
    assert.equal(activity.summary, "activity:read-only");
    assert.ok(activity.scope.includes(`run:${RUN}`));
    assert.ok(activity.scope.includes("step:review"));

    const beforeArtifact = JSON.parse(run(["room", "--json"])).data.room;
    assert.deepEqual(beforeArtifact.active_claims, [], "read activity must create zero active claims");
    const resume = workstreamStatus(
      { workstream: "empirical-read", tasks: [READ_ONLY_TASK] },
      beforeArtifact,
      { toolPrefix: "agent" },
    );
    assert.deepEqual(resume.active, ["review"]);
    assert.deepEqual(resume.claimed, []);
    assert.deepEqual(resume.to_dispatch, []);

    const artifactIdx = lines.findIndex((line) => line.startsWith("rally say artifact"));
    const artifactFull = `${lines[artifactIdx].replace(/\\\s*$/, "").trim()} ${lines[artifactIdx + 1].trim()}`
      .replace("<artifact-uri>", "review-notes.md")
      .replace("<verbatim verification output>", "true")
      .replace("<activity-event-id>", activity.event_id);
    const artifact = JSON.parse(run([...railsToArgv(artifactFull), "--json"])).data.say.fact;
    assert.equal(artifact.kind, "artifact");
    assert.equal(artifact.subject, "review: review notes");
    assert.equal(artifact.ref, activity.event_id);
    assert.ok(artifact.scope.includes(`run:${RUN}`));
    assert.ok(artifact.scope.includes("step:review"));

    const afterArtifact = JSON.parse(run(["room", "--json"])).data.room;
    assert.deepEqual(afterArtifact.active_claims, [], "terminal artifact must not leave ownership behind");
    assert.ok(afterArtifact.recent_artifacts.some((fact) => fact.event_id === artifact.event_id));
  } finally {
    rmSync(room, { recursive: true, force: true });
  }
});

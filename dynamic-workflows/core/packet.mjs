#!/usr/bin/env node
// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
// Portions adapted from pi-dynamic-workflows (MIT) by Michael Liverant,
// via github.com/tyroneross/pi-dynamic-workflows-fork. See ../NOTICE.
// Lifted: the descriptor field vocabulary (owns/validation/output) and the
// determinism discipline. NOT lifted: any execution machinery — this tool
// EMITS text for a host to act on; it never spawns, schedules, or runs an agent.

/**
 * packet — render a ready-to-paste subagent prompt packet from a workstream descriptor.
 *
 * This is the mechanical version of the step a frontier orchestrator used to do by
 * hand: turn each linted descriptor task into a self-contained prompt that a host's
 * OWN spawn mechanism (subagent tool / new terminal / paste to a human) consumes.
 * It is host-neutral text out — Rally facilitates, the host executes.
 *
 * Each emitted packet embeds, for ONE task:
 *   - the task intent
 *   - its owns paths (the write boundary)
 *   - the exact rally per-task loop with --run/--step/--parent-step already filled in
 *   - the validation command
 *   - the output contract
 *   - the "final message = one structured JSON result, no prose after" discipline
 *
 * Determinism: no Date.now()/Math.random()/new Date() — same descriptor + run_id in,
 * byte-identical packets out, so two hosts produce the same fan-out.
 *
 * Usage:
 *   node packet.mjs <descriptor.json> --run <run_id> [--task <id>] [--out <dir>] [--tool-prefix <p>]
 *
 * Flags:
 *   --run <run_id>      REQUIRED. The lineage handle threaded through every rally fact.
 *   --task <id>         Render only the named task (default: every task).
 *   --out <dir>         Write one <id>.packet.md file per task into <dir> (default: stdout).
 *   --tool-prefix <p>   Rally <TOOL> id prefix; the per-task id is "<p>:<task-id>"
 *                       (default prefix "agent" → "agent:<task-id>").
 *
 * Exit: 0 ok · 2 usage/parse/validation error.
 */

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { lintWorkstream } from "./workstream-lint.mjs";

function isNonEmptyString(v) {
  return typeof v === "string" && v.trim().length > 0;
}

/** Normalize a task's `owns` into an array (empty for read-only). Mirrors the linter. */
function ownedPaths(owns) {
  if (Array.isArray(owns)) return owns.filter(isNonEmptyString);
  return [];
}

/** The rally <TOOL> id for a task: "<prefix>:<task-id>". */
export function toolIdFor(taskId, prefix) {
  return `${prefix}:${taskId}`;
}

/**
 * Render the per-task rally loop with --run/--step/--parent-step already substituted.
 * The markers are what make the fan-out observable; bake them in so the spawned agent
 * cannot drop them.
 */
function renderRallyLoop({ task, runId, tool }) {
  const owns = ownedPaths(task.owns);
  const ownsArgs = owns.length ? owns.map((p) => `--path ${p}`).join(" ") : "";
  const deps = Array.isArray(task.depends_on) ? task.depends_on : [];
  // one --parent-step per depends_on entry; omit entirely if none
  const parentSteps = deps.map((d) => `--parent-step ${d}`).join(" ");
  const claimPath = owns.length ? ` ${ownsArgs}` : "";
  const checkLine = owns.length
    ? `rally check before-write --tool ${tool} ${ownsArgs} --strict`
    : `# read-only task — no before-write check required`;
  const artifactUri = owns.length ? owns[0] : "<artifact-uri>";
  const stepMarkers = `--run ${runId} --step ${task.id}${parentSteps ? " " + parentSteps : ""}`;

  return [
    `rally enter --tool ${tool}`,
    `rally say claim --tool ${tool} --subject "${task.intent}"${claimPath} \\`,
    `  ${stepMarkers}`,
    checkLine,
    `# blocking finding → stop; resolve or pick a non-overlapping task`,
    ``,
    `# do the work, then verify with the validation command below`,
    task.validation,
    ``,
    `rally say artifact --tool ${tool} --subject "${task.output}" --uri ${artifactUri} \\`,
    `  --evidence "<verbatim validation output>" --run ${runId} --step ${task.id}`,
    `rally say release --tool ${tool} --ref <claim-id> --subject "done"`,
    `rally next --tool ${tool}`,
  ].join("\n");
}

/** Render the full prompt packet (markdown) for one task. Pure function of its inputs. */
export function renderPacket({ task, runId, toolPrefix }) {
  const tool = toolIdFor(task.id, toolPrefix);
  const owns = ownedPaths(task.owns);
  const ownsList =
    task.owns === "read-only"
      ? "read-only (this task writes nothing it must claim)"
      : owns.map((p) => `- ${p}`).join("\n");
  const deps = Array.isArray(task.depends_on) ? task.depends_on : [];
  const depsLine = deps.length
    ? `Wait for these tasks to land first: ${deps.join(", ")}. They are your --parent-step lineage.`
    : "No upstream dependencies — start immediately.";

  return `# Task packet — ${task.id}

You are one agent in a rally-coordinated fan-out (run_id: \`${runId}\`). Do exactly
your assigned task and nothing outside your write boundary. Rally records and lints;
you do the work and report one structured result.

## Intent

${task.intent}

## Write boundary (owns)

You may write ONLY these paths. Touching any other path is a boundary violation —
stop and re-claim instead.

${ownsList}

${depsLine}

## Rally loop (run these verbatim — markers are pre-filled)

\`\`\`bash
${renderRallyLoop({ task, runId, tool })}
\`\`\`

The \`--run\`/\`--step\`/\`--parent-step\` markers are what let the orchestrator
reconstruct the fan-out with \`rally dag --run ${runId}\`. Do not drop them.

## Validation (deterministic — must pass before you post the artifact)

\`\`\`bash
${task.validation}
\`\`\`

## Output contract

Your work must produce: ${task.output}

## Final-message discipline

Your FINAL message is exactly ONE structured JSON result, with NO prose after it:

\`\`\`json
{ "task": "${task.id}", "changed_files": [${owns.map((p) => `"${p}"`).join(", ")}], "validation_result": "<verbatim output of the validation command>" }
\`\`\`

Nothing after that block. The orchestrator collects results and re-runs validation
for any shared-impact change — it does not trust your result without verifying.
`;
}

/** Parse argv into { file, runId, task, out, toolPrefix } or throw a usage Error. */
export function parseArgs(argv) {
  const args = argv.slice(2);
  let file = null;
  let runId = null;
  let task = null;
  let out = null;
  let toolPrefix = "agent";
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === "--run") runId = args[++i];
    else if (a === "--task") task = args[++i];
    else if (a === "--out") out = args[++i];
    else if (a === "--tool-prefix") toolPrefix = args[++i];
    else if (!a.startsWith("--") && file === null) file = a;
    else throw new Error(`unexpected argument: ${a}`);
  }
  if (!isNonEmptyString(file)) throw new Error("missing <descriptor.json>");
  if (!isNonEmptyString(runId)) throw new Error("--run <run_id> is required");
  if (!isNonEmptyString(toolPrefix)) throw new Error("--tool-prefix must be non-empty");
  return { file, runId, task, out, toolPrefix };
}

/**
 * Render packets for a descriptor doc. Returns [{ id, tool, content }].
 * Throws if the descriptor is invalid or the named --task does not exist.
 */
export function renderAll({ doc, runId, task, toolPrefix }) {
  const lintErrors = lintWorkstream(doc);
  if (lintErrors.length > 0) {
    throw new Error(
      `descriptor fails workstream-lint (${lintErrors.length}) — lint clean before generating packets:\n  - ${lintErrors.join("\n  - ")}`,
    );
  }
  let tasks = doc.tasks;
  if (isNonEmptyString(task)) {
    tasks = doc.tasks.filter((t) => t && t.id === task);
    if (tasks.length === 0) {
      throw new Error(`--task "${task}" not found in descriptor`);
    }
  }
  return tasks.map((t) => ({
    id: t.id,
    tool: toolIdFor(t.id, toolPrefix),
    content: renderPacket({ task: t, runId, toolPrefix }),
  }));
}

// ---- CLI ----
function main(argv) {
  let parsed;
  try {
    parsed = parseArgs(argv);
  } catch (err) {
    process.stderr.write(`${err.message}\n`);
    process.stderr.write(
      "usage: node packet.mjs <descriptor.json> --run <run_id> [--task <id>] [--out <dir>] [--tool-prefix <p>]\n",
    );
    return 2;
  }
  let raw;
  try {
    raw = readFileSync(parsed.file, "utf8");
  } catch (err) {
    process.stderr.write(`cannot read ${parsed.file}: ${err.message}\n`);
    return 2;
  }
  let doc;
  try {
    doc = JSON.parse(raw);
  } catch (err) {
    process.stderr.write(`invalid JSON in ${parsed.file}: ${err.message}\n`);
    return 2;
  }
  let packets;
  try {
    packets = renderAll({
      doc,
      runId: parsed.runId,
      task: parsed.task,
      toolPrefix: parsed.toolPrefix,
    });
  } catch (err) {
    process.stderr.write(`${err.message}\n`);
    return 2;
  }
  if (parsed.out) {
    mkdirSync(parsed.out, { recursive: true });
    for (const p of packets) {
      const dest = join(parsed.out, `${p.id}.packet.md`);
      writeFileSync(dest, p.content, "utf8");
      process.stdout.write(`wrote ${dest}\n`);
    }
  } else {
    process.stdout.write(
      packets.map((p) => p.content).join("\n\n---\n\n"),
    );
  }
  return 0;
}

// Run as CLI only when invoked directly (not when imported by tests).
if (import.meta.url === `file://${process.argv[1]}`) {
  process.exit(main(process.argv));
}

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
 *   - how the task is to be verified
 *   - the output contract
 *   - the "final message = one structured JSON result, no prose after" discipline
 *
 * WHAT GOES IN A ```bash BLOCK, AND WHAT DOES NOT. Only two kinds of command text
 * reach a runnable block: the rally loop this file writes, and the argv of a named
 * recipe from VALIDATION_RECIPES in workstream-lint.mjs. Both live in local source.
 * A descriptor's free-text `validation` field is NOT a command — it renders as a
 * quoted ```text block that the receiving agent must translate into a command it
 * takes responsibility for, under its own host's approval policy. A descriptor never
 * supplies text that a reader is told to run verbatim.
 *
 * Every value interpolated into an emitted command goes through `shellQuote`, so a
 * value cannot break out of its argument even if it never passed the linter.
 *
 * Determinism: no Date.now()/Math.random()/new Date() — same descriptor + run_id in,
 * byte-identical packets out, so two hosts produce the same fan-out.
 *
 * Usage:
 *   node packet.mjs <descriptor.json> --run <run_id> [--task <id>] [--out <dir>] [--tool-prefix <p>]
 *
 * Flags:
 *   --run <run_id>      REQUIRED. The lineage handle threaded through every rally fact.
 *                       Must match /^[A-Za-z0-9._-]+$/.
 *   --task <id>         Render only the named task (default: every task).
 *   --out <dir>         Write one <id>.packet.md file per task into <dir> (default: stdout).
 *   --tool-prefix <p>   Rally <TOOL> id prefix; the per-task id is "<p>:<task-id>"
 *                       (default prefix "agent" → "agent:<task-id>").
 *                       Must match /^[A-Za-z0-9._-]+$/.
 *
 * Exit: 0 ok · 2 usage/parse/validation error.
 */

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { lintWorkstream, IDENTIFIER_RE, lookupRecipe, VALIDATION_RECIPE_NAMES } from "./workstream-lint.mjs";

function isNonEmptyString(v) {
  return typeof v === "string" && v.trim().length > 0;
}

/**
 * Characters that need no quoting in any POSIX shell: no metacharacter, no
 * whitespace, no glob character, no quote. A token made only of these is already
 * exactly one argument.
 */
const SHELL_SAFE_BARE_RE = /^[A-Za-z0-9._/:@%+=,-]+$/;

/**
 * Render one value as exactly ONE POSIX shell argument.
 *
 * Safe tokens pass through bare so the emitted commands stay readable. Everything
 * else is wrapped in single quotes, with each embedded single quote closed, escaped,
 * and reopened (`'\''`) — the standard shlex.quote construction. Inside single
 * quotes the shell expands nothing: no variables, no command substitution, no globs,
 * no escapes. The result is a single argument for ANY input byte sequence, so this
 * function does not depend on the linter having run.
 */
export function shellQuote(value) {
  const s = String(value);
  if (s.length === 0) return "''";
  if (SHELL_SAFE_BARE_RE.test(s)) return s;
  return `'${s.replaceAll("'", "'\\''")}'`;
}

/**
 * Throw unless `value` is an identifier safe to render into command text.
 *
 * Called by parseArgs AND by renderAll/renderPacket. That duplication is the point:
 * a library caller that never goes through the CLI must not be able to skip it.
 */
export function assertIdentifier(field, value) {
  if (typeof value !== "string" || !IDENTIFIER_RE.test(value)) {
    throw new Error(
      `${field} must match /^[A-Za-z0-9._-]+$/ — it is rendered into command text (got: ${JSON.stringify(value)})`,
    );
  }
  return value;
}

/**
 * Pick a markdown fence longer than any backtick run in `body`, so the body cannot
 * close the block early and start one of its own. CommonMark allows fences of four
 * or more backticks for exactly this.
 */
function fenceFor(body) {
  const runs = String(body).match(/`+/g) ?? [];
  const longest = runs.reduce((max, r) => Math.max(max, r.length), 0);
  return "`".repeat(Math.max(3, longest + 1));
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
 * The argv for a task's named recipe, or null if it declares none (or `none`).
 * The command text comes from the local registry; the task only supplies the name
 * and, for path-scoped recipes, its already-allowlisted `owns` paths.
 */
export function recipeArgvFor(task) {
  const name = task?.validation_recipe;
  if (name === undefined) return null;
  const recipe = lookupRecipe(name);
  if (!recipe) {
    throw new Error(
      `validation_recipe "${name}" is not a known recipe (${VALIDATION_RECIPE_NAMES.join(" | ")})`,
    );
  }
  if (recipe.argv.length === 0) return null;
  const argv = [...recipe.argv];
  if (recipe.appendOwnedPaths) argv.push(...ownedPaths(task.owns));
  return argv;
}

/**
 * Render the per-task rally loop with --run/--step/--parent-step already substituted.
 * The markers are what make the fan-out observable; bake them in so the spawned agent
 * cannot drop them.
 */
function renderRallyLoop({ task, runId, tool, recipeArgv }) {
  const q = shellQuote;
  const owns = ownedPaths(task.owns);
  // CLI flag-arity asymmetry: `rally say` (the claim line below) ACCEPTS repeated
  // --path, so we join all owned paths into ONE claim. `rally check before-write`
  // REJECTS repeated --path, so checkLine emits one line per path instead. Same
  // owns list, two emission shapes — keep them in sync if the CLI arity changes.
  const ownsArgs = owns.length ? owns.map((p) => `--path ${q(p)}`).join(" ") : "";
  const deps = Array.isArray(task.depends_on) ? task.depends_on : [];
  // one --parent-step per depends_on entry; omit entirely if none.
  // `rally say` accepts repeated --parent-step (each becomes one DAG edge).
  const parentSteps = deps.map((d) => `--parent-step ${q(d)}`).join(" ");
  const claimPath = owns.length ? ` ${ownsArgs}` : "";
  // `rally check before-write` rejects repeated --path ("argument --path cannot be
  // used multiple times"), so emit ONE before-write line per owned path — unlike the
  // claim line above, where --path IS repeatable.
  const checkLine = owns.length
    ? owns.map((p) => `rally check before-write --tool ${q(tool)} --path ${q(p)} --strict`).join("\n")
    : `# read-only task — no before-write check required`;
  const artifactUri = owns.length ? q(owns[0]) : "<artifact-uri>";
  const stepMarkers = `--run ${q(runId)} --step ${q(task.id)}${parentSteps ? " " + parentSteps : ""}`;

  // The verify step. A named recipe resolves to argv from the LOCAL registry, so it
  // is safe to print as a command. A descriptor's free-text `validation` is not a
  // command and never appears here — it is prose, rendered in a ```text block below.
  const verifyLines = recipeArgv
    ? [`# verify — argv from the local recipe registry, not from the descriptor`, recipeArgv.map(q).join(" ")]
    : [`# verify — see "How to verify" below. Translate the description into a command`,
       `# YOU choose and take responsibility for, under this host's approval policy.`];

  return [
    `rally enter --tool ${q(tool)}`,
    `rally say claim --tool ${q(tool)} --subject ${q(task.intent)}${claimPath} \\`,
    `  ${stepMarkers}`,
    checkLine,
    `# blocking finding → stop; resolve or pick a non-overlapping task`,
    ``,
    `# do the work, then verify`,
    ...verifyLines,
    ``,
    `rally say artifact --tool ${q(tool)} --subject ${q(task.output)} --uri ${artifactUri} \\`,
    `  --evidence ${q("<verbatim verification output>")} --run ${q(runId)} --step ${q(task.id)}`,
    `rally say release --tool ${q(tool)} --ref <claim-id> --subject ${q("done")}`,
    `rally next --tool ${q(tool)}`,
  ].join("\n");
}

/** Render the full prompt packet (markdown) for one task. Pure function of its inputs. */
export function renderPacket({ task, runId, toolPrefix }) {
  // Defence in depth: these three are rendered into command text, so validate them
  // HERE too, not only in parseArgs. A library caller reaching renderPacket directly
  // gets the same check the CLI gets.
  assertIdentifier("--run <run_id>", runId);
  assertIdentifier("--tool-prefix", toolPrefix);
  assertIdentifier("task.id", task?.id);
  const recipeArgv = recipeArgvFor(task);
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

  return renderPacketBody({ task, runId, tool, owns, ownsList, depsLine, recipeArgv });
}

/** The verification section. Split out so the two sources of truth stay obvious. */
function renderVerification({ task, recipeArgv }) {
  const parts = [];
  if (recipeArgv) {
    parts.push(
      `The recipe \`${task.validation_recipe}\` resolves to this command. The text comes from`,
      `this repo's local recipe registry (\`core/workstream-lint.mjs\`), not from the descriptor,`,
      `so it is safe to run as written:`,
      ``,
      "```bash",
      recipeArgv.map(shellQuote).join(" "),
      "```",
    );
  }
  if (isNonEmptyString(task.validation)) {
    const fence = fenceFor(task.validation);
    if (recipeArgv) parts.push(``, `The descriptor also describes verification in prose:`, ``);
    parts.push(
      fence + "text",
      task.validation,
      fence,
      ``,
      `That block is a DESCRIPTION, not a command. It was written by whoever wrote the`,
      `descriptor and has not been checked by anything. Do not paste it into a shell.`,
      `Work out the command yourself, take responsibility for it, and run it under this`,
      `host's normal approval policy.`,
    );
  }
  if (parts.length === 0) {
    parts.push(`This task declares no verification. Say so plainly in your artifact evidence.`);
  }
  return parts.join("\n");
}

function renderPacketBody({ task, runId, tool, owns, ownsList, depsLine, recipeArgv }) {
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

Every line below was written by this generator, not by the descriptor, and every
value in it is shell-quoted. Run them as they stand.

\`\`\`bash
${renderRallyLoop({ task, runId, tool, recipeArgv })}
\`\`\`

The \`--run\`/\`--step\`/\`--parent-step\` markers are what let the orchestrator
reconstruct the fan-out with \`rally dag --run ${shellQuote(runId)}\`. Do not drop them.

## How to verify (must pass before you post the artifact)

${renderVerification({ task, recipeArgv })}

## Output contract

Your work must produce: ${task.output}

## Final-message discipline

Your FINAL message is exactly ONE structured JSON result, with NO prose after it:

\`\`\`json
{ "task": ${JSON.stringify(task.id)}, "changed_files": ${JSON.stringify(owns)}, "validation_result": "<verbatim output of the command you ran>" }
\`\`\`

Nothing after that block. The orchestrator collects results and re-verifies any
shared-impact change — it does not trust your result without checking.
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
  // Both are rendered into command text. Allowlist them at the boundary; renderAll
  // and renderPacket check again for callers that never come through here.
  assertIdentifier("--run <run_id>", runId);
  assertIdentifier("--tool-prefix", toolPrefix);
  if (task !== null) assertIdentifier("--task <id>", task);
  return { file, runId, task, out, toolPrefix };
}

/**
 * Render packets for a descriptor doc. Returns [{ id, tool, content }].
 * Throws if the descriptor is invalid or the named --task does not exist.
 */
export function renderAll({ doc, runId, task, toolPrefix }) {
  // Checked here as well as in parseArgs and renderPacket: a library caller must not
  // be able to reach the renderer with an identifier the CLI would have refused.
  assertIdentifier("--run <run_id>", runId);
  assertIdentifier("--tool-prefix", toolPrefix);
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

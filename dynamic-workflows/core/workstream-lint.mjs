#!/usr/bin/env node
// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
// Portions adapted from pi-dynamic-workflows (MIT) by Michael Liverant,
// via github.com/tyroneross/pi-dynamic-workflows-fork. See ../NOTICE.
// Lifted: the determinism blocklist and the literal-descriptor validation
// discipline (evaluateLiteral/validateMeta). NOT lifted: the vm executor,
// subagent runtime, or any execution machinery — Rally facilitates, never executes.

/**
 * workstream-lint — validate a JSON workstream descriptor before fan-out.
 *
 * A workstream is a coordination plan, not an execution engine. This linter is
 * the guardrail that makes a descriptor safe to hand to several agents:
 *   1. structural completeness  (every task declares owns + validation + output)
 *   2. determinism              (no Date.now()/Math.random()/new Date() in commands)
 *   3. MECE boundaries          (no two write-tasks own overlapping paths)
 *   4. dependency integrity     (depends_on ids resolve; no cycles)
 *
 * Usage:  node workstream-lint.mjs <descriptor.json>
 * Exit:   0 clean · 1 lint violations · 2 usage/parse error
 */

import { readFileSync } from "node:fs";

// Lifted verbatim from pi-dynamic-workflows/src/workflow.ts (MIT). A declared
// command that embeds wall-clock or randomness is not reproducible across the
// agents that share this workstream.
const DETERMINISM_BLOCKLIST = /\bDate\s*\.\s*now\b|\bMath\s*\.\s*random\b|\bnew\s+Date\s*\(\s*\)/;

const TIERS = new Set(["host-native", "cross-host"]);

function isNonEmptyString(v) {
  return typeof v === "string" && v.trim().length > 0;
}

/** Normalize a task's `owns` into an array of owned path strings (empty for read-only). */
function ownedPaths(owns) {
  if (owns === "read-only") return [];
  if (Array.isArray(owns)) return owns.filter(isNonEmptyString);
  return [];
}

/** Two path/glob tokens conflict if either is a prefix of the other (dir-segment aware). */
function pathsOverlap(a, b) {
  if (a === b) return true;
  // norm() here strips a trailing glob (`/*`, `/**`) and trailing slashes so that
  // "src/" and "src/*" both reduce to "src" for PREFIX-OVERLAP detection. It deliberately
  // does NOT strip a leading `file:` scheme — that is workstream-status's concern (exact-match),
  // not the lint's (MECE boundary). The two norm() helpers are intentionally different; do not merge.
  const norm = (p) => p.replace(/\/+$/, "").replace(/\/?\*+$/, "");
  const na = norm(a);
  const nb = norm(b);
  if (na === nb) return true;
  const longer = na.length >= nb.length ? na : nb;
  const shorter = na.length >= nb.length ? nb : na;
  return longer === shorter || longer.startsWith(shorter + "/");
}

export function lintWorkstream(doc) {
  const errors = [];
  const e = (msg) => errors.push(msg);

  if (!doc || typeof doc !== "object" || Array.isArray(doc)) {
    return ["descriptor must be a JSON object"];
  }
  if (!isNonEmptyString(doc.workstream)) e("`workstream` must be a non-empty string (the objective)");
  if (!isNonEmptyString(doc.description)) e("`description` must be a non-empty string");
  if (doc.thread !== undefined && !isNonEmptyString(doc.thread)) e("`thread`, if present, must be a non-empty string (rally thread id)");

  if (!Array.isArray(doc.tasks) || doc.tasks.length === 0) {
    e("`tasks` must be a non-empty array");
    return errors; // nothing more to check without tasks
  }

  const ids = new Set();
  const writeOwners = []; // { id, path } for MECE overlap detection

  doc.tasks.forEach((task, i) => {
    const where = `tasks[${i}]`;
    if (!task || typeof task !== "object" || Array.isArray(task)) {
      e(`${where} must be an object`);
      return;
    }
    if (!isNonEmptyString(task.id)) e(`${where}.id must be a non-empty string`);
    // packet.mjs interpolates `id` bare into `--step <id>`/`--parent-step <id>`
    // and into output filenames. Constrain it to a safe token charset so it can
    // never inject shell tokens or path separators into the emitted commands.
    else if (!/^[A-Za-z0-9._-]+$/.test(task.id)) {
      e(`${where}.id "${task.id}" must match /^[A-Za-z0-9._-]+$/ — it is interpolated bare into --step/--parent-step and packet filenames`);
    } else if (ids.has(task.id)) e(`${where}.id duplicates an earlier task id: ${task.id}`);
    else ids.add(task.id);

    const label = isNonEmptyString(task.id) ? task.id : where;

    if (!isNonEmptyString(task.intent)) e(`task ${label}: \`intent\` must be a non-empty string`);
    // packet.mjs interpolates `intent` inside a double-quoted bash --subject. A
    // double-quote, `$`, or backtick would break out of that quoting (or trigger
    // shell expansion / command substitution) in the emitted rally command.
    else if (/["$`]/.test(task.intent)) {
      e(`task ${label}: \`intent\` must not contain " $ or backtick — they break the emitted bash --subject quoting`);
    }
    if (!isNonEmptyString(task.validation)) e(`task ${label}: \`validation\` must be a non-empty string (how to verify the task)`);
    if (!isNonEmptyString(task.output)) e(`task ${label}: \`output\` must be a non-empty string (expected result shape)`);
    // packet.mjs interpolates `output` inside a double-quoted bash --subject at
    // the `rally say artifact` line, exactly like `intent` on the claim line. The
    // same characters break out of that quoting (or trigger shell expansion /
    // command substitution) in the emitted rally command.
    else if (/["$`]/.test(task.output)) {
      e(`task ${label}: \`output\` must not contain " $ or backtick — they break the emitted bash --subject quoting`);
    }

    // owns: required — either the literal "read-only" or a non-empty array of path strings
    const owns = task.owns;
    const ownsValid = owns === "read-only" || (Array.isArray(owns) && owns.length > 0 && owns.every(isNonEmptyString));
    if (!ownsValid) {
      e(`task ${label}: \`owns\` must be "read-only" or a non-empty array of path strings`);
    } else {
      for (const p of ownedPaths(owns)) {
        // packet.mjs emits owns paths bare (unquoted) as `--path <p>` on the
        // rally claim + before-write lines. Whitespace would split one path into
        // multiple shell tokens (silently claiming the wrong boundary); a quote,
        // `$`, or backtick would break quoting or trigger shell expansion /
        // command substitution in those bare positions.
        if (/[\s"$`]/.test(p)) {
          e(`task ${label}: \`owns\` path "${p}" must not contain whitespace, " $ or backtick — it is emitted bare as a --path token in the rally claim/before-write commands`);
        }
        writeOwners.push({ id: label, path: p });
      }
    }

    if (task.tier !== undefined && !TIERS.has(task.tier)) {
      e(`task ${label}: \`tier\` must be one of ${[...TIERS].join(" | ")}`);
    }

    // determinism: scan the declared validation string + every command for the blocklist
    if (isNonEmptyString(task.validation) && DETERMINISM_BLOCKLIST.test(task.validation)) {
      e(`task ${label}: \`validation\` is non-deterministic (Date.now()/Math.random()/new Date()) — declared commands must be reproducible`);
    }
    if (Array.isArray(task.commands)) {
      task.commands.forEach((cmd, ci) => {
        if (isNonEmptyString(cmd) && DETERMINISM_BLOCKLIST.test(cmd)) {
          e(`task ${label}: commands[${ci}] is non-deterministic (Date.now()/Math.random()/new Date())`);
        }
      });
    }
  });

  // MECE boundary check: no two write-tasks may own overlapping paths
  for (let a = 0; a < writeOwners.length; a++) {
    for (let b = a + 1; b < writeOwners.length; b++) {
      const x = writeOwners[a];
      const y = writeOwners[b];
      if (x.id !== y.id && pathsOverlap(x.path, y.path)) {
        e(`boundary conflict: task ${x.id} owns "${x.path}" overlaps task ${y.id} owns "${y.path}" — owns must be mutually exclusive`);
      }
    }
  }

  // dependency integrity: depends_on must resolve; no cycles
  const deps = new Map();
  doc.tasks.forEach((t) => {
    if (t && isNonEmptyString(t.id)) deps.set(t.id, Array.isArray(t.depends_on) ? t.depends_on : []);
  });
  for (const [id, list] of deps) {
    for (const d of list) {
      if (!deps.has(d)) e(`task ${id}: depends_on references unknown task id "${d}"`);
    }
  }
  // cycle detection (DFS)
  const WHITE = 0, GRAY = 1, BLACK = 2;
  const color = new Map([...deps.keys()].map((k) => [k, WHITE]));
  const stack = [];
  let cycle = null;
  const visit = (node) => {
    if (cycle) return;
    color.set(node, GRAY);
    stack.push(node);
    for (const d of deps.get(node) ?? []) {
      if (!deps.has(d)) continue;
      if (color.get(d) === GRAY) { cycle = [...stack.slice(stack.indexOf(d)), d]; return; }
      if (color.get(d) === WHITE) visit(d);
      if (cycle) return;
    }
    stack.pop();
    color.set(node, BLACK);
  };
  for (const id of deps.keys()) { if (color.get(id) === WHITE) visit(id); if (cycle) break; }
  if (cycle) e(`dependency cycle: ${cycle.join(" -> ")}`);

  return errors;
}

// ---- CLI ----
function main(argv) {
  const file = argv[2];
  if (!file) {
    process.stderr.write("usage: node workstream-lint.mjs <descriptor.json>\n");
    return 2;
  }
  let raw;
  try {
    raw = readFileSync(file, "utf8");
  } catch (err) {
    process.stderr.write(`cannot read ${file}: ${err.message}\n`);
    return 2;
  }
  let doc;
  try {
    doc = JSON.parse(raw);
  } catch (err) {
    process.stderr.write(`invalid JSON in ${file}: ${err.message}\n`);
    return 2;
  }
  const errors = lintWorkstream(doc);
  if (errors.length === 0) {
    process.stdout.write(`✓ ${file}: workstream descriptor is valid (${doc.tasks.length} task(s))\n`);
    return 0;
  }
  process.stderr.write(`✗ ${file}: ${errors.length} violation(s)\n`);
  for (const msg of errors) process.stderr.write(`  - ${msg}\n`);
  return 1;
}

// Run as CLI only when invoked directly (not when imported by tests).
if (import.meta.url === `file://${process.argv[1]}`) {
  process.exit(main(process.argv));
}

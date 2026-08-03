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
 * A workstream is a coordination plan, not an execution engine. This linter
 * checks four things:
 *   1. structural completeness  (every task declares owns + validation + output)
 *   2. determinism              (no Date.now()/Math.random()/new Date() in commands)
 *   3. MECE boundaries          (no two write-tasks own overlapping paths)
 *   4. dependency integrity     (depends_on ids resolve; no cycles)
 *
 * It also constrains the charset of the identifiers and paths that packet.mjs
 * renders into shell command text, so a rendered command cannot be broken out of.
 *
 * WHAT THIS LINTER IS NOT: it is not a security boundary. A clean lint does NOT
 * mean a descriptor is safe to execute. It does not read the code a task will
 * touch, it does not sandbox anything, and it cannot tell a helpful task from a
 * hostile one. `validation`, `description`, `intent`, and `output` are free prose
 * written by whoever wrote the descriptor — treat a descriptor from an author you
 * do not trust as untrusted input, exactly like a pull request from a stranger.
 * Running anything a descriptor describes is the host's decision, under the host's
 * own approval policy.
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

/**
 * Named validation recipes — the ONLY command text this module will ever render
 * into a runnable block.
 *
 * A descriptor author picks a recipe by NAME (`"validation_recipe": "cargo-test"`).
 * The argv lives here, in local source, under this repo's review. A descriptor can
 * never supply command text that reaches a bash block; that is the whole point of
 * the indirection. Adding a recipe is a code change, reviewed like any other.
 *
 * `appendOwnedPaths` lets a file-scoped tool receive the task's own `owns` paths as
 * arguments. The renderer shell-quotes each one; they are already charset-limited by
 * the `owns` allowlist below.
 */
export const VALIDATION_RECIPES = Object.freeze({
  none: Object.freeze({
    argv: Object.freeze([]),
    description: "no automated check — the agent states in its artifact evidence how it verified",
  }),
  "cargo-test": Object.freeze({
    argv: Object.freeze(["cargo", "test"]),
    description: "Rust workspace test suite",
  }),
  "cargo-clippy": Object.freeze({
    argv: Object.freeze(["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]),
    description: "Rust lint, warnings denied",
  }),
  "npm-test": Object.freeze({
    argv: Object.freeze(["npm", "test"]),
    description: "the package's own npm test script",
  }),
  "node-test": Object.freeze({
    argv: Object.freeze(["node", "--test"]),
    description: "Node built-in test runner",
  }),
  pytest: Object.freeze({
    argv: Object.freeze(["python3", "-m", "pytest", "-q"]),
    description: "pytest, quiet",
  }),
  "go-test": Object.freeze({
    argv: Object.freeze(["go", "test", "./..."]),
    description: "Go test suite",
  }),
  shellcheck: Object.freeze({
    argv: Object.freeze(["shellcheck"]),
    appendOwnedPaths: true,
    description: "shellcheck over the task's owned paths",
  }),
});

/** Recipe names, sorted — for error messages and docs. */
export const VALIDATION_RECIPE_NAMES = Object.freeze(Object.keys(VALIDATION_RECIPES).sort());

/** Look a recipe up without inheriting anything from Object.prototype. */
export function lookupRecipe(name) {
  if (typeof name !== "string") return null;
  return Object.hasOwn(VALIDATION_RECIPES, name) ? VALIDATION_RECIPES[name] : null;
}

/**
 * Identifiers rendered bare into command text (task ids, run ids, tool prefixes).
 * Positive allowlist: letters, digits, dot, underscore, hyphen. Nothing else — no
 * whitespace, no quotes, no shell metacharacter, no path separator, no control char.
 */
export const IDENTIFIER_RE = /^[A-Za-z0-9._-]+$/;

/** C0 controls + DEL. These have no place in a one-line command argument. */
const CONTROL_CHARS_RE = /[\u0000-\u001F\u007F]/;
/** Same, but tolerating LF — for fields that may legitimately span lines. */
const CONTROL_CHARS_NO_LF_RE = /[\u0000-\u0009\u000B-\u001F\u007F]/;

/** Characters allowed anywhere in an `owns` path, before the structural checks. */
const OWNS_CHARSET_RE = /^[A-Za-z0-9._/*-]+$/;
/** A single path segment: one or more allowlisted chars, no glob. */
const OWNS_SEGMENT_RE = /^[A-Za-z0-9._-]+$/;
/** The LAST segment may end in a `*` or `**` glob (`docs/*`, `src/**`, `plan-*`). */
const OWNS_LAST_SEGMENT_RE = /^[A-Za-z0-9._-]*\*{0,2}$/;

function isNonEmptyString(v) {
  return typeof v === "string" && v.trim().length > 0;
}

/**
 * Validate one `owns` path against the positive allowlist.
 * Returns null if acceptable, else a human-readable reason.
 *
 * Rejects, among everything else not on the allowlist: `;` `|` `&` `>` `<` `(` `)`
 * `$` backtick, quotes, whitespace, backslash, newline, and any control character —
 * so a path can never append a second command to a rendered `--path` argument.
 * Also rejects absolute paths and any `..` segment, so a path cannot escape the repo.
 */
export function ownsPathProblem(p) {
  if (typeof p !== "string" || p.length === 0) return "must be a non-empty string";
  if (!OWNS_CHARSET_RE.test(p)) {
    return "must contain only [A-Za-z0-9._/-] plus a trailing * or ** glob — no whitespace, quotes, or shell metacharacters";
  }
  if (p.startsWith("/")) return "must be repo-relative, not absolute (leading /)";
  if (p.endsWith("/")) return "must not end in a path separator";
  const segments = p.split("/");
  for (let i = 0; i < segments.length; i++) {
    const seg = segments[i];
    if (seg.length === 0) return "must not contain an empty path segment (//)";
    if (seg === "." || seg === "..") return "must not contain a . or .. segment (path escape)";
    const last = i === segments.length - 1;
    if (last) {
      if (!OWNS_LAST_SEGMENT_RE.test(seg)) {
        return "may only use * or ** as a trailing glob on the last segment";
      }
    } else if (!OWNS_SEGMENT_RE.test(seg)) {
      return "may only use * or ** as a trailing glob on the last segment";
    }
  }
  return null;
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

    // `intent` and `output` are prose that packet.mjs renders into a --subject
    // argument. The renderer shell-quotes them (packet.mjs `shellQuote`), so quoting
    // characters cannot break the token — but a newline or control character would
    // still corrupt a one-line command and mangle the recorded fact, so reject those
    // here. The `" $ backtick` rejection stays as defence in depth: a subject that
    // needs none of them is one fewer thing for a reviewer to reason about.
    if (!isNonEmptyString(task.intent)) e(`task ${label}: \`intent\` must be a non-empty string`);
    else if (/["$`]/.test(task.intent)) {
      e(`task ${label}: \`intent\` must not contain " $ or backtick — it is rendered into a bash --subject argument`);
    } else if (CONTROL_CHARS_RE.test(task.intent)) {
      e(`task ${label}: \`intent\` must not contain a newline or control character — it is rendered into a one-line bash --subject argument`);
    }
    if (!isNonEmptyString(task.validation)) e(`task ${label}: \`validation\` must be a non-empty string (how to verify the task)`);
    if (!isNonEmptyString(task.output)) e(`task ${label}: \`output\` must be a non-empty string (expected result shape)`);
    else if (/["$`]/.test(task.output)) {
      e(`task ${label}: \`output\` must not contain " $ or backtick — it is rendered into a bash --subject argument`);
    } else if (CONTROL_CHARS_RE.test(task.output)) {
      e(`task ${label}: \`output\` must not contain a newline or control character — it is rendered into a one-line bash --subject argument`);
    }

    // owns: required — either the literal "read-only" or a non-empty array of path strings
    const owns = task.owns;
    const ownsValid = owns === "read-only" || (Array.isArray(owns) && owns.length > 0 && owns.every(isNonEmptyString));
    if (!ownsValid) {
      e(`task ${label}: \`owns\` must be "read-only" or a non-empty array of path strings`);
    } else {
      for (const p of ownedPaths(owns)) {
        // Positive allowlist (see ownsPathProblem). packet.mjs also shell-quotes each
        // path, so these two checks are independent: the allowlist keeps the declared
        // boundary readable and repo-relative; the quoting keeps the rendered token
        // intact even for a path that never passed through this linter.
        const problem = ownsPathProblem(p);
        if (problem) {
          e(`task ${label}: \`owns\` path "${p}" ${problem}`);
        }
        writeOwners.push({ id: label, path: p });
      }
    }

    if (task.tier !== undefined && !TIERS.has(task.tier)) {
      e(`task ${label}: \`tier\` must be one of ${[...TIERS].join(" | ")}`);
    }

    // validation_recipe: optional. It names a recipe from the LOCAL registry above;
    // an unknown name is a hard error rather than a silent fall-through to prose,
    // because the recipe is the only path by which a task gets a runnable command.
    if (task.validation_recipe !== undefined) {
      if (!lookupRecipe(task.validation_recipe)) {
        e(
          `task ${label}: \`validation_recipe\` must be one of ${VALIDATION_RECIPE_NAMES.join(" | ")} — recipes are defined locally in workstream-lint.mjs; a descriptor cannot supply command text`,
        );
      }
    }

    // determinism: scan the declared validation string + every command for the blocklist
    if (isNonEmptyString(task.validation) && DETERMINISM_BLOCKLIST.test(task.validation)) {
      e(`task ${label}: \`validation\` is non-deterministic (Date.now()/Math.random()/new Date()) — declared commands must be reproducible`);
    }
    // `validation` renders inside a fenced markdown block. A backtick run of three or
    // more is an attempt (deliberate or accidental) to close that fence early and
    // inject its own block — including a ```bash block the reader would then trust.
    // The renderer widens its fence to survive this; reject it here as well so the
    // descriptor itself stays reviewable.
    if (isNonEmptyString(task.validation) && /```/.test(task.validation)) {
      e(`task ${label}: \`validation\` must not contain a triple-backtick fence — it would break out of the markdown block the packet renders it in`);
    }
    if (isNonEmptyString(task.validation) && CONTROL_CHARS_NO_LF_RE.test(task.validation)) {
      e(`task ${label}: \`validation\` must not contain control characters`);
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

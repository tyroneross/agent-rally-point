// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
// Adapted from pi-dynamic-workflows (MIT) src/workflow.ts — parallel(), pipeline(), and the
// budget object. See ../NOTICE.
// Source fork: github.com/tyroneross/pi-dynamic-workflows-fork
// Upstream:    github.com/Michaelliv/pi-dynamic-workflows
//
// HOST-NEUTRAL control-flow primitives. This module owns concurrency, ordering, and budget
// accounting only. It does NOT import node:vm, typebox, WorkflowAgent, or any
// @mariozechner/* package. The host supplies actual agent-spawn logic as thunk/stage bodies.

export { createLimiter } from "./limiter.mjs";

// ---------------------------------------------------------------------------
// parallel(thunks, opts?)
// ---------------------------------------------------------------------------
/**
 * Run an array of zero-argument async thunks concurrently. Results are
 * returned in input order. A thunk that throws resolves to null; the error
 * is forwarded to `onError` when provided. An AbortSignal causes any
 * in-flight thunks that have already thrown an AbortError to re-throw
 * immediately instead of mapping to null.
 *
 * @param {Array<() => Promise<unknown>>} thunks
 * @param {{ signal?: AbortSignal, onError?: (index: number, err: unknown) => void }} [opts]
 * @returns {Promise<Array<unknown>>}
 */
export async function parallel(thunks, { signal, onError } = {}) {
  if (!Array.isArray(thunks)) throw new TypeError("parallel() expects an array of thunks");
  if (thunks.some((t) => typeof t !== "function")) {
    throw new TypeError("parallel() expects an array of functions, not promises. Wrap each call: () => yourFn(...)");
  }
  return Promise.all(
    thunks.map(async (thunk, index) => {
      try {
        return await thunk();
      } catch (err) {
        // Re-throw on abort so the caller can surface the abort.
        if (signal?.aborted) throw err;
        onError?.(index, err);
        return null;
      }
    }),
  );
}

// ---------------------------------------------------------------------------
// pipeline(items, ...stages)
// ---------------------------------------------------------------------------
/**
 * Thread each item independently through one or more stage functions.
 * Stages receive (currentValue, originalItem, index). All items run in
 * parallel; stages within one item run in series. A stage error for a given
 * item short-circuits that item and resolves it to null (other items unaffected).
 *
 * IMPROVEMENT over the pi original (durable-model): an optional trailing options
 * object — `pipeline(items, stageA, stageB, { signal, onError })` — threads an
 * AbortSignal (re-throw on abort instead of swallowing) and an `onError(index, err)`
 * callback. **Observe `onError` to checkpoint the failure to Rally** — a failed item
 * resolves to `null`, indistinguishable in the result array from a stage that legitimately
 * returned null; the side-channel is what lets `workstream-status.mjs` tell "re-dispatch
 * this" from "skip this". Without it, pi silently conflates failure with absence.
 *
 * @param {unknown[]} items
 * @param {...((prev: unknown, original: unknown, index: number) => unknown) | { signal?: AbortSignal, onError?: (index: number, err: unknown) => void }} stagesThenOpts
 * @returns {Promise<Array<unknown>>}
 */
export async function pipeline(items, ...stagesThenOpts) {
  if (!Array.isArray(items)) throw new TypeError("pipeline() expects an array as the first argument");
  // Peel an optional trailing options object, preserving pi's variadic form. Only a plain
  // object counts as opts; a non-function non-object (e.g. a bad stage) stays in `stages`
  // so the function-validation below still rejects it.
  let opts = {};
  const last = stagesThenOpts[stagesThenOpts.length - 1];
  if (last !== null && typeof last === "object" && !Array.isArray(last)) {
    opts = stagesThenOpts.pop();
  }
  const stages = stagesThenOpts;
  if (stages.some((s) => typeof s !== "function")) {
    throw new TypeError("pipeline() stages must be functions: pipeline(items, item => ..., result => ...)");
  }
  const { signal, onError } = opts;
  return Promise.all(
    items.map(async (item, index) => {
      let value = item;
      for (const stage of stages) {
        if (signal?.aborted) throw new Error("pipeline aborted");
        try {
          value = await stage(value, item, index);
        } catch (err) {
          if (signal?.aborted) throw err; // surface abort instead of swallowing
          onError?.(index, err); // record the failure (e.g. checkpoint to Rally) so resume re-dispatches
          return null;
        }
      }
      return value;
    }),
  );
}

// ---------------------------------------------------------------------------
// budget(total?)
// ---------------------------------------------------------------------------
/**
 * Standalone budget counter. Closure over a mutable counter so the same
 * reference can be passed to multiple helpers and stays in sync. Units are the
 * host's choice (tokens, API calls, dollars) — this module is host-neutral and
 * does NOT bake in pi's `len/4` token heuristic; the host calls add() with whatever
 * it measures.
 *
 * @param {number | null} [total]  cap in the host's units, or null / undefined for unlimited
 * @returns {{ total: number|null, spent: () => number, add: (n: number) => void, remaining: () => number }}
 */
export function budget(total = null) {
  let _spent = 0;
  return Object.freeze({
    total: total ?? null,
    spent: () => _spent,
    add: (n) => { _spent += n; },
    remaining: () => (total == null ? Infinity : Math.max(0, total - _spent)),
  });
}

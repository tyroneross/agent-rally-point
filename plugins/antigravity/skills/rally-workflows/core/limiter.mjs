// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
// Adapted from pi-dynamic-workflows (MIT) src/workflow.ts createLimiter(). See ../NOTICE.
// A bounded-concurrency helper for hosts that want to cap their OWN Tier-1 fan-out.
// This is host-side scaffolding — it never runs inside Rally and owns no agent lifecycle.

/**
 * Create a concurrency limiter. `limit` in-flight at most; the rest queue FIFO.
 *
 *   const run = createLimiter(4);
 *   await Promise.all(tasks.map((t) => run(() => doTask(t))));
 *
 * @param {number} limit  maximum simultaneous executions (clamped to >= 1)
 * @returns {<T>(fn: () => Promise<T>) => Promise<T>}
 */
export function createLimiter(limit) {
  const max = Math.max(1, Math.floor(limit) || 1);
  let active = 0;
  const queue = [];
  const next = () => {
    active--;
    queue.shift()?.();
  };
  return async (fn) => {
    if (active >= max) await new Promise((resolve) => queue.push(resolve));
    active++;
    try {
      return await fn();
    } finally {
      next();
    }
  };
}

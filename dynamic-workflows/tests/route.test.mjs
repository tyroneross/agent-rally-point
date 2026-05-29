// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { parallel, pipeline, budget } from "../core/route.mjs";
import * as routeModule from "../core/route.mjs";
import { createLimiter } from "../core/limiter.mjs";

// ---------------------------------------------------------------------------
// parallel
// ---------------------------------------------------------------------------
describe("parallel", () => {
  it("preserves input order", async () => {
    const results = await parallel([
      () => Promise.resolve("a"),
      () => Promise.resolve("b"),
      () => Promise.resolve("c"),
    ]);
    assert.deepEqual(results, ["a", "b", "c"]);
  });

  it("maps a throwing thunk to null", async () => {
    const results = await parallel([
      () => Promise.resolve(1),
      () => Promise.reject(new Error("boom")),
      () => Promise.resolve(3),
    ]);
    assert.deepEqual(results, [1, null, 3]);
  });

  it("calls onError with index and error when a thunk throws", async () => {
    const errors = [];
    await parallel(
      [() => Promise.reject(new Error("x"))],
      { onError: (i, e) => errors.push([i, e.message]) },
    );
    assert.deepEqual(errors, [[0, "x"]]);
  });

  it("throws TypeError for non-array input", async () => {
    await assert.rejects(() => parallel("oops"), TypeError);
  });

  it("throws TypeError when items are not functions", async () => {
    await assert.rejects(() => parallel([Promise.resolve(1)]), TypeError);
  });

  it("returns empty array for empty input", async () => {
    assert.deepEqual(await parallel([]), []);
  });
});

// ---------------------------------------------------------------------------
// pipeline
// ---------------------------------------------------------------------------
describe("pipeline", () => {
  it("threads items through stages independently", async () => {
    const results = await pipeline(
      [1, 2, 3],
      (v) => v * 10,
      (v) => v + 1,
    );
    assert.deepEqual(results, [11, 21, 31]);
  });

  it("isolates a failing item to null, leaving others intact", async () => {
    const results = await pipeline(
      [1, 2, 3],
      (v) => {
        if (v === 2) throw new Error("bad item");
        return v * 10;
      },
    );
    assert.deepEqual(results, [10, null, 30]);
  });

  it("passes (currentValue, originalItem, index) to each stage", async () => {
    const log = [];
    await pipeline(
      ["a", "b"],
      (cur, orig, idx) => { log.push([cur, orig, idx]); return cur.toUpperCase(); },
      (cur, orig, idx) => { log.push([cur, orig, idx]); return cur + "!"; },
    );
    // Items run concurrently so entries may interleave across items.
    // Assert only the per-item stage ordering guarantee:
    //   stage-1 call for an item always precedes stage-2 call for the same item.
    const item0 = log.filter(([, orig]) => orig === "a");
    assert.equal(item0.length, 2, "item 0 should have exactly 2 stage calls");
    assert.equal(item0[0][0], "a", "item 0 stage-1 receives original value");
    assert.equal(item0[1][0], "A", "item 0 stage-2 receives stage-1 result");
    assert.equal(item0[0][2], 0, "item 0 index is 0");

    const item1 = log.filter(([, orig]) => orig === "b");
    assert.equal(item1.length, 2, "item 1 should have exactly 2 stage calls");
    assert.equal(item1[0][0], "b", "item 1 stage-1 receives original value");
    assert.equal(item1[1][0], "B", "item 1 stage-2 receives stage-1 result");
    assert.equal(item1[0][2], 1, "item 1 index is 1");
  });

  it("throws TypeError for non-array items", async () => {
    await assert.rejects(() => pipeline("not an array", (x) => x), TypeError);
  });

  it("throws TypeError when a stage is not a function", async () => {
    await assert.rejects(() => pipeline([1, 2], "not a fn"), TypeError);
  });

  it("returns empty array for empty items", async () => {
    assert.deepEqual(await pipeline([], (x) => x), []);
  });

  // --- durable-model improvements over pi (trailing opts) ---
  it("forwards a stage failure to onError (failure visibility for resume)", async () => {
    const errs = [];
    const results = await pipeline(
      [1, 2, 3],
      (v) => { if (v === 2) throw new Error("nope"); return v; },
      { onError: (i, e) => errs.push([i, e.message]) },
    );
    assert.deepEqual(results, [1, null, 3]);
    assert.deepEqual(errs, [[1, "nope"]], "failed item index+error surfaced so the host can checkpoint it");
  });

  it("re-throws on an aborted signal instead of swallowing", async () => {
    const ac = new AbortController();
    ac.abort();
    await assert.rejects(() => pipeline([1, 2], (v) => v, { signal: ac.signal }), /aborted/);
  });

  it("still accepts the pi-faithful variadic form with no trailing opts", async () => {
    assert.deepEqual(await pipeline([1, 2], (v) => v + 1), [2, 3]);
  });
});

// ---------------------------------------------------------------------------
// budget
// ---------------------------------------------------------------------------
describe("budget", () => {
  it("remaining() is Infinity when total is null", () => {
    const b = budget(null);
    assert.equal(b.total, null);
    assert.equal(b.remaining(), Infinity);
  });

  it("remaining() is Infinity when total is undefined (default)", () => {
    const b = budget();
    assert.equal(b.remaining(), Infinity);
  });

  it("tracks spent and remaining correctly", () => {
    const b = budget(100);
    assert.equal(b.spent(), 0);
    assert.equal(b.remaining(), 100);

    b.add(40);
    assert.equal(b.spent(), 40);
    assert.equal(b.remaining(), 60);

    b.add(60);
    assert.equal(b.spent(), 100);
    assert.equal(b.remaining(), 0);
  });

  it("remaining() floors at 0 when overspent", () => {
    const b = budget(10);
    b.add(20);
    assert.equal(b.remaining(), 0);
  });

  it("total is frozen on the returned object", () => {
    const b = budget(50);
    assert.equal(b.total, 50);
    // Frozen — silently ignores reassignment in strict mode (no-op)
    try { b.total = 999; } catch (_) { /* expected */ }
    assert.equal(b.total, 50);
  });
});

// ---------------------------------------------------------------------------
// createLimiter (canonical home: ./limiter.mjs — NOT re-exported from ./route)
// ---------------------------------------------------------------------------
describe("createLimiter", () => {
  it("is not re-exported from ./route (single canonical path is ./limiter)", () => {
    assert.equal(routeModule.createLimiter, undefined, "createLimiter must not be re-exported from route.mjs");
  });

  it("caps concurrency at the given limit", async () => {
    const limit = 2;
    const run = createLimiter(limit);
    let active = 0;
    let maxObserved = 0;

    const tasks = Array.from({ length: 6 }, (_, i) =>
      run(async () => {
        active++;
        maxObserved = Math.max(maxObserved, active);
        // Yield to the event loop so other tasks can start if under limit
        await new Promise((r) => setImmediate(r));
        active--;
        return i;
      }),
    );

    const results = await Promise.all(tasks);
    assert.equal(maxObserved, limit, `max concurrent was ${maxObserved}, expected <= ${limit}`);
    assert.deepEqual(results, [0, 1, 2, 3, 4, 5]);
  });

  it("resolves all tasks even when limit is 1", async () => {
    const run = createLimiter(1);
    const order = [];
    await Promise.all([0, 1, 2].map((n) => run(async () => { order.push(n); })));
    assert.deepEqual(order, [0, 1, 2]);
  });
});

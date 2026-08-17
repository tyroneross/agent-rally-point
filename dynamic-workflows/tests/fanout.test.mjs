// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

import { test } from "node:test";
import assert from "node:assert/strict";
import { resolveFanout, liveAgentsFromRoom, DEFAULT_MAX, HARD_CEILING } from "../core/fanout.mjs";
import { createLimiter } from "../core/limiter.mjs";

const squad = (tool, freshness, status) => ({ tool, freshness, status });
const roomWith = (...squads) => ({ data: { room: { squads } } });

test("defaults to DEFAULT_MAX when nothing is supplied", () => {
  const r = resolveFanout();
  assert.equal(r.effective_max, DEFAULT_MAX);
  assert.deepEqual(r.limiting_factors, ["requested_or_config"]);
});

test("DEFAULT_MAX is 10 and stays under the hard ceiling", () => {
  assert.equal(DEFAULT_MAX, 10);
  assert.ok(DEFAULT_MAX <= HARD_CEILING, "default must not exceed the ceiling");
});

test("the hard ceiling clamps an over-large request and says so", () => {
  const r = resolveFanout({ requested: 50 });
  assert.equal(r.effective_max, HARD_CEILING);
  assert.deepEqual(r.limiting_factors, ["hard_ceiling"]);
});

test("requested overrides configMax", () => {
  const r = resolveFanout({ requested: 3, configMax: 9 });
  assert.equal(r.effective_max, 3);
  assert.equal(r.config_max, 9);
});

test("a host resource cap binds below the config max", () => {
  const r = resolveFanout({ configMax: 10, hostCap: 4 });
  assert.equal(r.effective_max, 4);
  assert.deepEqual(r.limiting_factors, ["host"]);
});

test("never dispatches more agents than there are ready tasks", () => {
  const r = resolveFanout({ configMax: 10, readyTasks: 2 });
  assert.equal(r.effective_max, 2);
  assert.deepEqual(r.limiting_factors, ["ready_tasks"]);
});

test("reports every constraint that ties for the binding value", () => {
  const r = resolveFanout({ configMax: 5, hostCap: 5, readyTasks: 5 });
  assert.equal(r.effective_max, 5);
  assert.deepEqual(r.limiting_factors, ["host", "ready_tasks", "requested_or_config"]);
});

test("omitted host cap and ready-task count do not appear as constraints", () => {
  const r = resolveFanout({ configMax: 6 });
  assert.deepEqual(Object.keys(r.caps).sort(), ["hard_ceiling", "requested_or_config"]);
  assert.equal(r.host_cap, null);
  assert.equal(r.ready_tasks, null);
});

test("floors at 1 for zero, negative, and non-integer inputs", () => {
  for (const bad of [0, -5, 2.5, null, undefined, "4", NaN]) {
    const r = resolveFanout({ requested: bad, configMax: bad, hostCap: bad, readyTasks: bad });
    assert.ok(r.effective_max >= 1, `expected >= 1 for input ${String(bad)}`);
  }
  assert.equal(resolveFanout({ configMax: 10, hostCap: 0 }).effective_max, 10);
});

test("counts only fresh AND active squads as live agents", () => {
  const live = liveAgentsFromRoom(
    roomWith(
      squad("a:1", "fresh", "active"),
      squad("a:2", "fresh", "idle"), // present but holding nothing
      squad("a:3", "stale", "active"), // ended without stopping
      squad("a:4", "stale", "idle"),
    ),
  );
  assert.equal(live.count, 1);
  assert.deepEqual(live.tools, ["a:1"]);
});

test("excludes your own tool ids so a fan-out does not subtract itself", () => {
  const room = roomWith(
    squad("orchestrator", "fresh", "active"),
    squad("orchestrator:p01", "fresh", "active"),
    squad("peer:other", "fresh", "active"),
  );
  assert.equal(liveAgentsFromRoom(room).count, 3);
  const live = liveAgentsFromRoom(room, { excludeTools: ["orchestrator", "orchestrator:p01"] });
  assert.equal(live.count, 1);
  assert.deepEqual(live.tools, ["peer:other"]);
});

test("accepts every room envelope shape and tolerates a missing squads list", () => {
  const squads = [squad("a:1", "fresh", "active")];
  assert.equal(liveAgentsFromRoom({ data: { room: { squads } } }).count, 1);
  assert.equal(liveAgentsFromRoom({ room: { squads } }).count, 1);
  assert.equal(liveAgentsFromRoom({ squads }).count, 1);
  assert.equal(liveAgentsFromRoom({}).count, 0);
  assert.equal(liveAgentsFromRoom(null).count, 0);
});

test("live peers consume headroom out of the config max", () => {
  const r = resolveFanout({ configMax: 10, liveAgents: 4 });
  assert.equal(r.effective_max, 6);
  assert.deepEqual(r.limiting_factors, ["room_headroom"]);
  assert.equal(r.live_agents, 4);
});

test("a saturated room floors at 1 rather than deadlocking the workstream", () => {
  const r = resolveFanout({ configMax: 10, liveAgents: 50 });
  assert.equal(r.effective_max, 1);
  assert.deepEqual(r.limiting_factors, ["room_headroom"]);
});

test("an empty room adds no headroom constraint at all", () => {
  const r = resolveFanout({ configMax: 8, liveAgents: 0 });
  assert.ok(!("room_headroom" in r.caps), "no live peers should not create a cap");
  assert.equal(r.effective_max, 8);
});

test("room headroom composes with the other caps", () => {
  // config 10 - 2 live = 8 headroom, but only 3 tasks are ready.
  const r = resolveFanout({ configMax: 10, liveAgents: 2, readyTasks: 3 });
  assert.equal(r.effective_max, 3);
  assert.deepEqual(r.limiting_factors, ["ready_tasks"]);
  assert.equal(r.caps.room_headroom, 8);
});

test("the resolved width actually bounds a limiter's in-flight count", async () => {
  const { effective_max } = resolveFanout({ configMax: 10, readyTasks: 25 });
  const run = createLimiter(effective_max);
  let active = 0;
  let peak = 0;
  await Promise.all(
    Array.from({ length: 25 }, () =>
      run(async () => {
        peak = Math.max(peak, ++active);
        await new Promise((resolve) => setTimeout(resolve, 5));
        active--;
      }),
    ),
  );
  assert.equal(peak, effective_max, `expected peak in-flight ${effective_max}, saw ${peak}`);
});

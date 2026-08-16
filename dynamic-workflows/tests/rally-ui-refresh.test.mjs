// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { runInNewContext } from "node:vm";

const INDEX_HTML = fileURLToPath(
  new URL("../../crates/rally-ui/assets/index.html", import.meta.url),
);
const html = readFileSync(INDEX_HTML, "utf8");

function extractFunction(name) {
  const start = html.indexOf(`  function ${name}(`);
  assert.notEqual(start, -1, `${name} must exist in the shipped HTML`);
  const body = html.indexOf("{", start);
  let depth = 0;
  for (let i = body; i < html.length; i += 1) {
    if (html[i] === "{") depth += 1;
    if (html[i] === "}") depth -= 1;
    if (depth === 0) return html.slice(start, i + 1).trim();
  }
  assert.fail(`${name} must have a balanced body`);
}

function loadFunction(name, context = {}) {
  return runInNewContext(`(${extractFunction(name)})`, context);
}

test("coalesces concurrent refresh requests into one sequential follow-up", async () => {
  const createCoalescedRunner = loadFunction("createCoalescedRunner");
  let calls = 0;
  let active = 0;
  let maxActive = 0;
  const releases = [];
  const request = createCoalescedRunner(() => {
    calls += 1;
    active += 1;
    maxActive = Math.max(maxActive, active);
    return new Promise((resolve) => {
      releases.push(() => {
        active -= 1;
        resolve(calls);
      });
    });
  });

  const first = request();
  const second = request();
  const third = request();
  assert.equal(first, second);
  assert.equal(second, third);
  await Promise.resolve();
  assert.equal(calls, 1);
  assert.equal(maxActive, 1);

  releases.shift()();
  await new Promise(setImmediate);
  assert.equal(calls, 2);
  assert.equal(active, 1);
  assert.equal(maxActive, 1);

  releases.shift()();
  await Promise.all([first, second, third]);
  assert.equal(calls, 2);
  assert.equal(active, 0);
});

test("does not render an older detail response after A to B to A selection", async () => {
  const rendered = [];
  const responses = [];
  const context = {
    API: { room: (id) => id },
    fetch: () => new Promise((resolve) => responses.push(resolve)),
    renderRoomDetail: (snapshot) => rendered.push(snapshot),
    state: { selectedId: "room-a", selectionGeneration: 3 },
  };
  const loadRoomDetail = loadFunction("loadRoomDetail", context);

  const stale = loadRoomDetail("room-a", 1);
  responses.shift()({ json: () => Promise.resolve({ marker: "stale" }) });
  await stale;
  assert.deepEqual(rendered, []);

  const current = loadRoomDetail("room-a", 3);
  responses.shift()({ json: () => Promise.resolve({ marker: "current" }) });
  await current;
  assert.equal(rendered.length, 1);
  assert.equal(rendered[0].marker, "current");
});

test("a queued refresh does not invalidate the current selection", async () => {
  const state = { reloadRoomsPending: false, selectionGeneration: 7 };
  const requestViewLoad = loadFunction("requestViewLoad", {
    runViewLoad: () => Promise.resolve(),
    state,
  });

  await requestViewLoad(true);
  assert.equal(state.reloadRoomsPending, true);
  assert.equal(state.selectionGeneration, 7);
});

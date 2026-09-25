// SPDX-License-Identifier: Apache-2.0
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const source = fileURLToPath(new URL("..", import.meta.url));

test("packed module installs offline with its license, docs, exports and working commands", () => {
  const scratch = mkdtempSync(join(tmpdir(), "rally flow package-"));
  const env = { ...process.env, npm_config_cache: join(scratch, "cache") };
  const run = (cmd, args, cwd = source, expected = 0) => {
    const result = spawnSync(cmd, args, { cwd, env, encoding: "utf8", timeout: 60_000 });
    assert.equal(result.error, undefined);
    assert.equal(result.status, expected, `${cmd} ${args.join(" ")}\n${result.stdout}\n${result.stderr}`);
    return result.stdout;
  };
  try {
    // A parent `npm publish --dry-run` must still exercise real temporary files.
    const [packed] = JSON.parse(run("npm", ["pack", "--dry-run=false", "--ignore-scripts", "--json", "--pack-destination", scratch]));
    const paths = packed.files.map((entry) => entry.path);
    assert.ok(paths.includes("LICENSE"), "published artifact must include the Apache license");
    assert.ok(paths.includes("NOTICE"), "published artifact must preserve third-party attribution");
    assert.ok(!paths.some((path) => path.startsWith("tests/")), "source tests are not consumer runtime files");
    const consumer = join(scratch, "consumer");
    run("npm", ["install", "--dry-run=false", "--global=false", "--prefix", consumer, "--offline", "--ignore-scripts", "--no-audit", "--no-fund", join(scratch, packed.filename)]);
    const installed = join(consumer, "node_modules", packed.name);
    const manifest = JSON.parse(readFileSync(join(installed, "package.json"), "utf8"));
    assert.equal(packed.name, "@tyroneross/rally-flow", "never publish under the unrelated dynamic-workflows name");
    assert.equal(readFileSync(join(installed, "LICENSE"), "utf8"), readFileSync(join(source, "..", "LICENSE"), "utf8"));
    assert.match(readFileSync(join(installed, "NOTICE"), "utf8"), /MIT License/);
    for (const doc of ["README.md", "PROTOCOL.md", "COORDINATION.md", "MODEL-TIERS.md"]) {
      const text = readFileSync(join(installed, doc), "utf8");
      assert.doesNotMatch(text, /\]\(\.\.\//, `${doc} must not link outside the package using parent paths`);
    }
    for (const [subpath, target] of Object.entries(manifest.exports)) {
      assert.ok(existsSync(join(installed, target)));
      run(process.execPath, ["--input-type=module", "-e", `await import(${JSON.stringify(manifest.name + subpath.slice(1))})`], consumer);
    }
    const example = join(installed, "examples/audit-repo.workstream.json");
    const lint = join(consumer, "node_modules/.bin/workstream-lint");
    const status = join(consumer, "node_modules/.bin/workstream-status");
    assert.match(run(lint, [example], consumer), /valid/);
    run(lint, [join(installed, "examples/bad-missing-fields.workstream.json")], consumer, 1);
    run(process.execPath, [join(installed, "core/packet.mjs")], consumer, 2);
    run(process.execPath, [join(installed, "core/checkpoint.mjs")], consumer, 2);
    const room = join(scratch, "room.json");
    writeFileSync(room, JSON.stringify({ data: { room: {} } }));
    const state = JSON.parse(run(status, [example, room], consumer, 3));
    assert.equal(state.complete, false);
    assert.ok(state.to_dispatch.length > 0);
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});

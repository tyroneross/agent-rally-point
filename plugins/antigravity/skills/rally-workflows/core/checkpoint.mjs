#!/usr/bin/env node
// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
// Immutable task context. No latest pointer, background summarizer or second task board.
import { createHash } from 'node:crypto';
import { constants, openSync, closeSync, readSync, writeFileSync, fsyncSync, fstatSync, mkdirSync, linkSync, unlinkSync } from 'node:fs';
import { join, basename, resolve } from 'node:path';
import { isMain } from "./is-main.mjs";

export const MAX_BYTES = 65536;
// Logical ids include Build Loop's host-qualified run ids. Filenames use only
// content hashes, so a colon is metadata rather than a filesystem delimiter.
const ID = /^[A-Za-z0-9._:-]+$/;
const HASH = /^[a-f0-9]{64}$/;
const REVISION = /^[a-f0-9]{40,64}$/;
const digest = (s) => createHash('sha256').update(s).digest('hex');
function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === 'object') return Object.fromEntries(Object.keys(value).sort().map(k => [k, canonical(value[k])]));
  return value;
}
export function validateCheckpoint(c, expected = {}) {
  if (!c || c.schema !== 'agent-rally.checkpoint.v1') throw new Error('unsupported checkpoint schema');
  for (const key of ['run_id', 'task_id']) if (typeof c[key] !== 'string' || !ID.test(c[key]) || c[key].length > 128) throw new Error(`invalid ${key}`);
  if (!Number.isSafeInteger(c.generation) || c.generation < 1) throw new Error('invalid generation');
  if (!REVISION.test(c.revision ?? '')) throw new Error('revision must be a full git object id');
  for (const key of ['goal', 'next_action']) if (typeof c[key] !== 'string' || !c[key].trim()) throw new Error(`missing ${key}`);
  if (!Array.isArray(c.constraints) || c.constraints.some(s => typeof s !== 'string' || !s.trim())) throw new Error('invalid constraints');
  if (!Array.isArray(c.evidence) || c.evidence.some(e => !e || typeof e.uri !== 'string' || !e.uri.trim() || !HASH.test(e.sha256 ?? ''))) throw new Error('evidence needs uri and sha256');
  if (c.generation === 1 ? c.previous !== null : !HASH.test(c.previous ?? '')) throw new Error('invalid previous checkpoint');
  for (const key of ['run_id', 'task_id', 'revision']) if (expected[key] !== undefined && c[key] !== expected[key]) throw new Error(`checkpoint ${key} mismatch`);
  const serialized = JSON.stringify(canonical(c));
  if (Buffer.byteLength(serialized) > MAX_BYTES - 256) throw new Error('checkpoint exceeds byte limit');
  return serialized;
}
function readBounded(path) {
  const fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const st = fstatSync(fd);
    if (!st.isFile() || st.size > MAX_BYTES) throw new Error('checkpoint must be a bounded regular file');
    const data = Buffer.alloc(MAX_BYTES + 1);
    // A fixed buffer also bounds a file that grows after fstat.
    let n = 0;
    while (n < data.length) {
      const count = readSync(fd, data, n, data.length - n, n);
      if (count === 0) break;
      n += count;
    }
    if (n > MAX_BYTES) throw new Error('checkpoint exceeds byte limit');
    return JSON.parse(data.subarray(0, n).toString('utf8'));
  } finally { closeSync(fd); }
}
export function readCheckpoint(path, expected = {}) {
  const envelope = readBounded(path);
  const serialized = validateCheckpoint(envelope.checkpoint, expected);
  if (!HASH.test(envelope.sha256 ?? '') || digest(serialized) !== envelope.sha256) throw new Error('checkpoint digest mismatch');
  return envelope;
}
export function putCheckpoint(directory, checkpoint) {
  const serialized = validateCheckpoint(checkpoint);
  const sha256 = digest(serialized);
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  if (checkpoint.previous) {
    const prior = readCheckpoint(join(directory, `${checkpoint.previous}.json`), { run_id: checkpoint.run_id, task_id: checkpoint.task_id });
    if (prior.sha256 !== checkpoint.previous) throw new Error('checkpoint predecessor digest differs from requested hash');
    if (prior.checkpoint.generation + 1 !== checkpoint.generation) throw new Error('checkpoint generation gap');
  }
  const result = { sha256, checkpoint: canonical(checkpoint) };
  const path = join(directory, `${sha256}.json`);
  const temp = join(directory, `.${sha256}.${process.pid}.${createHash('sha256').update(String(process.hrtime.bigint())).digest('hex')}.tmp`);
  const fd = openSync(temp, 'wx', 0o600);
  try { writeFileSync(fd, `${JSON.stringify(result)}\n`); fsyncSync(fd); } finally { closeSync(fd); }
  try {
    try { linkSync(temp, path); } catch (e) {
      if (e.code !== 'EEXIST') throw e;
      if (readCheckpoint(path, checkpoint).sha256 !== sha256) throw new Error("existing checkpoint digest differs from filename");
    }
    const dirfd = openSync(directory, constants.O_RDONLY);
    try { fsyncSync(dirfd); } finally { closeSync(dirfd); }
  } finally { unlinkSync(temp); }
  return { path: resolve(path), sha256 };
}
export function main(argv) {
  try {
    const [command, path, source] = argv.slice(2);
    let result;
    if (command === 'put' && path && source) result = putCheckpoint(path, readBounded(source));
    else if (command === 'get' && path && !source) result = readCheckpoint(path);
    else throw new Error(`usage: ${basename(argv[1])} put <directory> <context.json> | get <checkpoint.json>`);
    process.stdout.write(`${JSON.stringify(result)}\n`);
    return 0;
  } catch (e) { process.stderr.write(`${e.message}\n`); return 2; }
}
if (isMain(import.meta.url)) process.exitCode = main(process.argv);

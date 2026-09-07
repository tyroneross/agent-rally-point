// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, readFileSync, writeFileSync, symlinkSync, readdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync, spawnSync } from 'node:child_process';
import { putCheckpoint, readCheckpoint, validateCheckpoint } from '../core/checkpoint.mjs';
const base = { schema:'agent-rally.checkpoint.v1', run_id:'pilot', task_id:'task', generation:1, revision:'a'.repeat(40), goal:'Preserve Unicode 日本語', constraints:['no ownership changes'], evidence:[], next_action:'Verify the artifact', previous:null };
test('host-qualified Build Loop run ids remain metadata',t=>{
  const dir=sandbox(t), c={...base,run_id:'bl-20260907T034452Z-codex:optimization-execution-460112'};
  const saved=putCheckpoint(dir,c);
  assert.deepEqual(readCheckpoint(saved.path,{run_id:c.run_id}).checkpoint,c);
});
function sandbox(t) { const dir=mkdtempSync(join(tmpdir(),'rally-capsule-')); t.after(()=>rmSync(dir,{recursive:true,force:true})); return dir; }
for (const host of ['codex','claude','rosslabs-agent-harness','gemini','cursor']) {
  test(`${host} reads the same lossless capsule contract`, t=>{
    const dir=sandbox(t), c={...base,producer:host};
    const first=putCheckpoint(dir,c), again=putCheckpoint(dir,c);
    assert.deepEqual(first,again);
    assert.deepEqual(readCheckpoint(first.path).checkpoint,c);
    assert.equal(readdirSync(dir).length,1);
  });
}
test('reject corruption, stale revision, scope mismatch, unsupported version and oversize',t=>{
  const dir=sandbox(t), {path}=putCheckpoint(dir,base);
  assert.throws(()=>readCheckpoint(path,{revision:'b'.repeat(40)}),/revision mismatch/);
  assert.throws(()=>readCheckpoint(path,{task_id:'other'}),/task_id mismatch/);
  assert.throws(()=>validateCheckpoint({...base,schema:'future'}),/schema/);
  assert.throws(()=>validateCheckpoint({...base,goal:'x'.repeat(65536)}),/byte limit/);
  const e=JSON.parse(readFileSync(path));e.checkpoint.goal='corrupt';writeFileSync(path,JSON.stringify(e));
  assert.throws(()=>readCheckpoint(path),/digest mismatch/);
});
test('generation chain must exist and match task',t=>{
  const dir=sandbox(t), one=putCheckpoint(dir,base);
  assert.throws(()=>putCheckpoint(dir,{...base,generation:3,previous:one.sha256}),/generation gap/);
  assert.throws(()=>putCheckpoint(dir,{...base,task_id:'other',generation:2,previous:one.sha256}),/mismatch/);
  const two=putCheckpoint(dir,{...base,generation:2,previous:one.sha256,next_action:'Done'});
  assert.equal(readCheckpoint(two.path).checkpoint.previous,one.sha256);
});
test('generation chain rejects a valid envelope stored under another digest',t=>{
  const dir=sandbox(t), one=putCheckpoint(dir,base);
  const other=putCheckpoint(dir,{...base,next_action:'Different predecessor'});
  writeFileSync(one.path,readFileSync(other.path));
  assert.throws(()=>putCheckpoint(dir,{...base,generation:2,previous:one.sha256}),/predecessor digest/);
});
test('partial writes and symlinks are refused',t=>{
  const dir=sandbox(t);writeFileSync(join(dir,'partial'),'{}');
  assert.throws(()=>readCheckpoint(join(dir,'partial')));
  const {path}=putCheckpoint(dir,base);symlinkSync(path,join(dir,'link'));
  assert.throws(()=>readCheckpoint(join(dir,'link')));
});
test('packet CLI actually consumes checkpoint and rejects stale context',t=>{
  const dir=sandbox(t), example=JSON.parse(readFileSync(new URL('../examples/audit-repo.workstream.json',import.meta.url)));
  const task=example.tasks[0].id;
  const {path}=putCheckpoint(dir,{...base,task_id:task});
  const packet=new URL('../core/packet.mjs',import.meta.url).pathname;
  const args=[packet,new URL('../examples/audit-repo.workstream.json',import.meta.url).pathname,'--run','pilot','--task',task,'--checkpoint',path,'--revision',base.revision];
  const out=execFileSync(process.execPath,args,{encoding:'utf8'});
  assert.match(out,/Preserve Unicode 日本語/);
  assert.match(out,/Resume context/);
  args[args.length-1]='b'.repeat(40);
  assert.equal(spawnSync(process.execPath,args).status,2);
});

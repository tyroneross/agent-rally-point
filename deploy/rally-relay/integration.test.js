// SPDX-License-Identifier: Apache-2.0
import assert from "node:assert/strict";
import {randomBytes,createHash} from "node:crypto";
import {execFile,spawn} from "node:child_process";
import {promisify} from "node:util";
import {mkdtemp,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {join} from "node:path";
import test from "node:test";
import {once} from "node:events";
import {createServer} from "node:net";
import {writeFile} from "node:fs/promises";

test("offline handoff, competing clients, durable restart and unattended execution", async t => {
  const dir = await mkdtemp(join(tmpdir(),"rally-relay-"));
  const registry = {};
  const tokens = {};
  for (const id of ["mac-a","mac-b","cloud","outsider"]) {
    tokens[id] = randomBytes(32).toString("hex");
    registry[createHash("sha256").update(tokens[id]).digest("hex")] = {
      id,rooms:[id === "outsider" ? "other" : "org:project"],capabilities:[id === "cloud" ? "verify" : "handoff"]
    };
  }
  const vars = join(dir,".dev.vars");
  await writeFile(vars,"ENROLLMENTS='" + JSON.stringify(registry) + "'\n",{mode:0o600});
  const probe = createServer();
  probe.listen(0,"127.0.0.1"); await once(probe,"listening");
  const port = probe.address().port;
  await new Promise(resolve=>probe.close(resolve));
  const url = "http://127.0.0.1:"+port;
  async function start() {
    const child = spawn("node_modules/.bin/wrangler",["dev","--local","--ip","127.0.0.1","--port",String(port),
      "--persist-to",dir,"--env-file",vars],{stdio:["ignore","pipe","pipe"]});
    let log = "";
    child.stdout.on("data",b=>log+=b); child.stderr.on("data",b=>log+=b);
    const closed = once(child,"close");
    for (let i=0;i<150;i++) {
      if (child.exitCode !== null) throw Error(log);
      try { if ((await fetch(url)).status === 404) return {ready:url, dispose:async()=>{child.kill();await closed;}}; } catch {}
      await new Promise(resolve=>setTimeout(resolve,100));
    }
    child.kill(); await closed; throw Error(log);
  }
  let mf = await start();
  t.after(async () => { await mf.dispose(); await rm(dir,{recursive:true,force:true}); });
  const call = async (id,action,body={}) => fetch(new URL("/rooms/org:project/"+action,await mf.ready),{
    method:"POST",headers:{authorization:"Bearer "+tokens[id]},body:JSON.stringify(body)
  });
  const ok = async (...args) => {
    const res = await call(...args);
    const data = await res.json(); assert.equal(res.status,200,JSON.stringify(data)); return data;
  };
  const item = {id:"offline",target:"mac-b",text:"Review the selected changes"};
  assert.equal((await ok("mac-a","members")).find(m=>m.id==="mac-b").recently_seen,false);
  assert.equal((await ok("mac-a","send",item)).state,"queued");
  assert.equal((await call("outsider","status",{id:item.id})).status,403);
  assert.equal((await call("unknown","status",{id:item.id})).status,401);
  assert.equal(await ok("mac-a","claim"),null);
  await mf.dispose(); mf=await start();
  assert.equal((await ok("mac-b","claim")).envelope.event_id,item.id);
  assert.equal((await call("mac-a","ack",item)).status,403);
  assert.equal((await ok("mac-b","ack",item)).state,"acknowledged");
  assert.equal((await ok("mac-b","ack",item)).state,"acknowledged");
  await ok("mac-b","complete",{id:item.id,result:"reviewed"});
  assert.equal((await ok("mac-a","send",item)).state,"completed");
  assert.equal((await call("mac-a","send",{...item,text:"different"})).status,409);
  await ok("mac-a","send",{...item,id:"race",target:"*"});
  const claims = await Promise.all(["mac-a","mac-b"].map(id=>ok(id,"claim")));
  assert.equal(claims.filter(Boolean).length,1);
  assert.equal((await ok(claims.find(Boolean).owner,"claim")).envelope.event_id,"race");
  const git = async (...args) => (await promisify(execFile)("git",["-C",dir,...args])).stdout.trim();
  await git("init","-q");
  await writeFile(join(dir,"work.txt"),"revision to verify\n");
  await git("add","work.txt");
  await git("-c","user.name=Relay Test","-c","user.email=test@example.invalid","-c","commit.gpgsign=false","commit","-qm","fixture");
  const revision = await git("rev-parse","HEAD");
  await ok("mac-a","send",{id:"night",text:revision,target:"*",capability:"verify"});
  assert.equal((await call("mac-a","claim",{capability:"verify"})).status,403);
  await promisify(execFile)("python3",["client.py","work","--capability","verify","--journal",join(dir,"worker.sqlite"),
    "--once","--","python3","verify_revision.py",dir],{
    env:{...process.env,RALLY_RELAY_URL:String(await mf.ready),RALLY_ROOM:"org:project",RALLY_RELAY_TOKEN:tokens.cloud}
  });
  const result = await ok("mac-a","status",{id:"night"});
  assert.equal(result.owner,"cloud");
  assert.deepEqual(JSON.parse(result.result),{exit:0,output:revision+"\n"});
});

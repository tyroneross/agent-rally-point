// SPDX-License-Identifier: Apache-2.0
import { DurableObject } from "cloudflare:workers";

const identifier = value => typeof value === "string" && /^[a-zA-Z0-9][\w.:-]{0,127}$/.test(value);
const reply = (value, status = 200) => Response.json(value, {status, headers: {"Cache-Control":"no-store"}});
const error = (message, status) => reply({error: message}, status);

export default {
  async fetch(request, env) {
    const path = new URL(request.url).pathname;
    const match = /^\/rooms\/([\w.:-]+)\/(send|claim|ack|complete|status|members)$/.exec(path);
    if (!match || !identifier(match[1])) return error("unknown route", 404);
    if (request.method !== "POST") return error("POST required", 405);
    const token = /^Bearer ([A-Za-z0-9_-]{32,128})$/.exec(request.headers.get("Authorization") || "");
    if (!token) return error("invalid credential", 401);
    const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(token[1]));
    const key = Array.from(new Uint8Array(digest), b => b.toString(16).padStart(2,"0")).join("");
    const registry = JSON.parse(env.ENROLLMENTS || "{}");
    const member = registry[key];
    if (!member) return error("invalid credential", 401);
    const [ , room, action ] = match;
    const members = Object.values(registry).filter(m => m.rooms.includes(room));
    if (!member.rooms.includes(room)) return error("room access denied", 403);
    const length = request.headers.get("content-length");
    if (length === null || Number(length) > 32768) return error("bounded request body required", 413);
    let data;
    try { data = await request.json(); } catch { return error("invalid JSON", 400); }
    if (!data || typeof data !== "object" || Array.isArray(data)) return error("object required", 400);
    return env.ROOMS.getByName(room).apply(room, member, members.map(m => m.id), action, data);
  }
};

export class Room extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    this.sql.exec(`CREATE TABLE IF NOT EXISTS items (
      id TEXT PRIMARY KEY, sender TEXT NOT NULL, target TEXT NOT NULL, capability TEXT NOT NULL,
      text TEXT NOT NULL, created REAL NOT NULL, owner TEXT, accepted INTEGER DEFAULT 0, result TEXT);
      CREATE INDEX IF NOT EXISTS pending ON items(owner, target, capability);
      CREATE TABLE IF NOT EXISTS presence (id TEXT PRIMARY KEY, seen INTEGER NOT NULL)`);
  }

  apply(room, member, members, action, data) {
    const sql = this.sql;
    const actor = member.id;
    const now = Date.now();
    sql.exec("INSERT INTO presence VALUES (?,?) ON CONFLICT(id) DO UPDATE SET seen=excluded.seen", actor, now);
    if (action === "members") {
      const seen = new Map(sql.exec("SELECT * FROM presence").toArray().map(r => [r.id,r.seen]));
      return reply(members.map(id => ({id, last_seen: seen.get(id) ?? null, recently_seen: now - (seen.get(id) ?? 0) < 60000})));
    }
    const {id, target, capability = "handoff", text} = data;
    if (action !== "claim" && !identifier(id)) return error("valid event id required", 400);
    const lookup = key => sql.exec("SELECT rowid AS seq,* FROM items WHERE id=?", key).toArray()[0];
    if (action === "send") {
      if (!(target === "*" || members.includes(target)) || !identifier(capability) ||
          typeof text !== "string" || !text.trim() || text.length > 16000) return error("invalid handoff", 400);
      const old = lookup(id);
      if (old && (old.sender !== actor || old.target !== target || old.capability !== capability || old.text !== text))
        return error("event id already used", 409);
      sql.exec("INSERT OR IGNORE INTO items(id,sender,target,capability,text,created) VALUES(?,?,?,?,?,?)",
        id,actor,target,capability,text,now/1000);
    }
    if (action === "claim") {
      if (!identifier(capability) || !member.capabilities.includes(capability)) return error("capability denied", 403);
      // Return outstanding work first, including after a lost claim response.
      const next = sql.exec(`SELECT id FROM items WHERE owner=? AND result IS NULL AND capability=?
        ORDER BY rowid LIMIT 1`,actor,capability).toArray()[0] ||
        sql.exec(`SELECT id FROM items WHERE owner IS NULL AND capability=? AND target IN (?, '*')
          ORDER BY rowid LIMIT 1`,capability,actor).toArray()[0];
      if (!next) return reply(null);
      sql.exec("UPDATE items SET owner=? WHERE id=? AND owner IS NULL",actor,next.id);
      return reply(this.view(room,lookup(next.id)));
    }
    const row = lookup(id);
    if (!row || ![row.sender,row.target,row.owner].includes(actor)) return error("unknown item",404);
    if (action === "ack" || action === "complete") {
      if (row.owner !== actor) return error("claimer required",403);
      if (action === "ack") sql.exec("UPDATE items SET accepted=1 WHERE id=?",id);
      else {
        if (!row.accepted) return error("acknowledge first",409);
        if (typeof data.result !== "string" || !data.result.length || data.result.length > 16000) return error("invalid result",400);
        if (row.result !== null && row.result !== data.result) return error("result already recorded",409);
        sql.exec("UPDATE items SET result=? WHERE id=?",data.result,id);
      }
    }
    return reply(this.view(room,lookup(id)));
  }

  view(room,row) {
    return {state: row.result !== null ? "completed" : row.accepted ? "acknowledged" : row.owner ? "claimed" : "queued",
      owner:row.owner, capability:row.capability, result:row.result, envelope:{
        delivery_schema:"rally.delivery.v1", repository_id:room, event_id:row.id, seq:row.seq,
        from:row.sender, to:row.target, kind:"deliver", type:"addition", text:row.text, ts:row.created
      }};
  }
}

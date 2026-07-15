// Remote-only conformance (contract §6): transactions (MULTI/EXEC/WATCH),
// pipeline, and declarative indexes (IDX.*). node --test + bun test.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { connect, ProtocolError, InvalidInputError, isError, replyText, type Client } from "../src/index.ts";
import { spawnServer, b, s, type Server } from "./harness.ts";

let server: Server;
before(async () => {
  server = await spawnServer();
});
after(() => server?.close());

async function fresh(): Promise<Client> {
  const c = await connect(server.url);
  await c.flushall();
  return c;
}

// --- Transactions (§3.12) -----------------------------------------------

test("MULTI → queue → EXEC returns N replies in order", async () => {
  const c = await fresh();
  const t = await c.multi();
  await t.set("a", "1");
  await t.incr("n");
  await t.get("a");
  const replies = await t.exec();
  assert.equal(replies.length, 3);
  assert.equal(replies[0]!.kind, "simple");
  assert.deepEqual(replies[1], { kind: "int", value: 1 });
  assert.equal(replyText(replies[2]!), "1");
  c.close();
});

test("typed builders + execTyped cursor + expectEmpty arity gate", async () => {
  const c = await fresh();
  const t = await c.multi();
  await t.set("a", "v");
  await t.incr("ctr");
  await t.get("a");
  const cur = await t.execTyped();
  cur.nextOK();
  assert.equal(cur.nextInt(), 1);
  assert.equal(s(cur.nextBulk()), "v");
  cur.expectEmpty();
  c.close();
});

test("WATCH + concurrent modify → execWatched null; execTyped abort → Protocol", async () => {
  const c = await fresh();
  const other = await connect(server.url);
  await c.watch("wk");
  await other.set("wk", "changed");
  const t = await c.multi();
  await t.set("wk", "mine");
  assert.equal(await t.execWatched(), null, "WATCH violation aborts");

  await c.watch("wk");
  await other.set("wk", "again");
  const t2 = await c.multi();
  await t2.incr("wk2");
  await assert.rejects(t2.execTyped(), (e) => e instanceof ProtocolError);
  c.close();
  other.close();
});

test("abandon-without-exec sends implicit DISCARD (socket not stuck)", async () => {
  const c = await fresh();
  const t = await c.multi();
  await t.set("x", "1");
  await t.close(); // implicit DISCARD
  await c.set("y", "2");
  assert.equal(s(await c.get("y")), "2", "socket usable after abandon");
  assert.equal(await c.get("x"), null, "abandoned write did not apply");
  c.close();
});

// --- Pipeline (§3.13) ---------------------------------------------------

test("pipeline: N commands one round-trip, replies in order", async () => {
  const c = await fresh();
  const replies = await c.pipeline((p) => {
    p.cmd("SET", "a", "1").cmd("INCR", "n").cmd("GET", "a");
  });
  assert.equal(replies.length, 3);
  assert.deepEqual(replies[1], { kind: "int", value: 1 });
  assert.equal(replyText(replies[2]!), "1");
  c.close();
});

test("pipeline: per-command -ERR lands inline; empty batch; empty argv", async () => {
  const c = await fresh();
  const inline = await c.pipeline((p) => {
    p.cmd("SET", "sv", "str").cmd("INCR", "sv").cmd("GET", "sv");
  });
  assert.ok(isError(inline[1]!), "per-command error inline, batch not aborted");
  assert.equal(replyText(inline[2]!), "str");

  assert.deepEqual(await c.pipeline(() => {}), [], "empty batch → no wire I/O");
  await assert.rejects(
    c.pipeline((p) => p.cmd()),
    (e) => e instanceof InvalidInputError,
  );
  c.close();
});

// --- IDX (§3.8) ---------------------------------------------------------

async function waitIdxReady(c: Client, name: string): Promise<void> {
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    for (const info of await c.idxList()) {
      if (s(info.name) === name && info.state === "ready") return;
    }
    await new Promise((r) => setTimeout(r, 20));
  }
  throw new Error(`index ${name} never became ready`);
}

test("idx create range → range paging + eq + list parse + drop", async () => {
  const c = await fresh();
  await c.idxCreateRange("byage", "user:", "age", "i64");
  const ages = ["21", "22", "23", "24", "25"];
  for (let i = 0; i < ages.length; i++) {
    await c.hset("user:" + String.fromCharCode(97 + i), "age", ages[i]!);
  }
  await waitIdxReady(c, "byage");

  const infos = await c.idxList();
  const byage = infos.find((in_) => s(in_.name) === "byage");
  assert.ok(byage, "byage present in IDX.LIST");
  assert.equal(byage!.kind, "range");

  let seen = 0;
  let cursor: Uint8Array | undefined;
  for (;;) {
    const page = await c.idxQueryRange("byage", "0", "100", 2, cursor);
    seen += page.rows.length;
    if (page.cursor == null) break;
    cursor = page.cursor;
    assert.ok(seen <= 10, "paging terminates");
  }
  assert.equal(seen, 5, "range paging saw all rows");

  const eq = await c.idxQueryEq("byage", "23", 10);
  assert.equal(eq.rows.length, 1);
  assert.equal(s(eq.rows[0]!.value), "23");

  assert.equal(await c.idxDrop("byage"), true);
  assert.equal(await c.idxDrop("byage"), false);
  c.close();
});

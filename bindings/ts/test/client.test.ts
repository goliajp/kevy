// Conformance (contract §6): connection routing, error mapping, core KV,
// collections, sorted-set algebra, hash-field TTL, blocking pops, and the
// sync/async agreement — run against BOTH the embedded (mem://) and remote
// (kevy://) backend. Runs on node --test and bun test.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import {
  connect,
  connectSync,
  StoreError,
  UnsupportedError,
  InvalidInputError,
  type Client,
} from "../src/index.ts";
import { spawnServer, uniqueMemUrl, b, s, type Server } from "./harness.ts";

let server: Server;
before(async () => {
  server = await spawnServer();
});
after(() => server?.close());

// Run a body against the embedded and remote backend in turn.
async function eachBackend(fn: (c: Client, name: string) => Promise<void>): Promise<void> {
  const ec = await connect(uniqueMemUrl());
  try {
    await fn(ec, "embedded");
  } finally {
    ec.close();
  }
  const rc = await connect(server.url);
  try {
    await rc.flushall();
    await fn(rc, "remote");
  } finally {
    rc.close();
  }
}

// --- Connection & URL routing (§6) --------------------------------------

test("mem:// is isolated; two are independent", async () => {
  const a = await connect("mem://");
  const c = await connect("mem://");
  await a.set("k", "A");
  assert.equal(await c.get("k"), null, "anonymous mem:// stores are independent");
  a.close();
  c.close();
});

test("mem://<name> opened twice shares one store", async () => {
  const url = uniqueMemUrl();
  const a = await connect(url);
  const c = await connect(url);
  await a.set("shared", "yes");
  assert.equal(s(await c.get("shared")), "yes");
  a.close();
  c.close();
});

test("TLS/AUTH/unknown/empty schemes rejected before I/O", async () => {
  await assert.rejects(connect("rediss://h"), (e) => e instanceof UnsupportedError);
  await assert.rejects(connect("kevys://h"), (e) => e instanceof UnsupportedError);
  await assert.rejects(connect("redis://user:pass@h"), (e) => e instanceof UnsupportedError);
  await assert.rejects(connect("floop://h"), (e) => e instanceof InvalidInputError);
  await assert.rejects(connect("file://"), (e) => e instanceof InvalidInputError);
});

test("sync + async faces exist on ONE client and agree (embedded)", async () => {
  const c = await connect(uniqueMemUrl());
  await c.set("k", "v");
  assert.equal(s(await c.get("k")), "v");
  assert.equal(s(c.sync.get("k")), "v", "sync face agrees with async");
  c.sync.set("k2", "w");
  assert.equal(s(await c.get("k2")), "w", "async sees sync writes");
  assert.equal(c.sync.incr("n"), 1);
  assert.equal(await c.incr("n"), 2, "faces share the same store");
  c.close();
});

test("connectSync is embedded-only; remote sync throws Unsupported", async () => {
  const c = connectSync(uniqueMemUrl());
  c.sync.set("a", "1");
  assert.equal(s(c.sync.get("a")), "1");
  c.close();
  const rc = await connect(server.url);
  assert.throws(() => rc.sync.get("x"), (e) => e instanceof UnsupportedError);
  rc.close();
});

// --- Error mapping (§6) -------------------------------------------------

test("wrong-type op → Store(WrongType); INCR non-numeric → Store(NotInteger)", async () => {
  await eachBackend(async (c) => {
    await c.set("str", "hello");
    await assert.rejects(
      c.lpush("str", "x"),
      (e) => e instanceof StoreError && e.storeKind === "wrongType",
    );
    await c.set("nan", "abc");
    await assert.rejects(
      c.incr("nan"),
      (e) => e instanceof StoreError && e.storeKind === "notInteger",
    );
  });
});

test("embedded IDX.*/MULTI/pipeline → Unsupported", async () => {
  const c = await connect(uniqueMemUrl());
  await assert.rejects(c.idxList(), (e) => e instanceof UnsupportedError);
  await assert.rejects(c.multi(), (e) => e instanceof UnsupportedError);
  await assert.rejects(c.pipeline(() => {}), (e) => e instanceof UnsupportedError);
  await assert.rejects(c.watch("k"), (e) => e instanceof UnsupportedError);
  c.close();
});

// --- Core KV (§6) -------------------------------------------------------

test("set/get/del/exists/incr/incrBy; missing → null", async () => {
  await eachBackend(async (c) => {
    assert.equal(await c.get("nope"), null);
    await c.set("k", "v");
    assert.equal(s(await c.get("k")), "v");
    assert.equal(await c.exists("k", "k", "nope"), 2, "repeated key counts each");
    assert.equal(await c.del("k"), 1);
    assert.equal(await c.incr("ctr"), 1);
    assert.equal(await c.incrBy("ctr", 4), 5);
    assert.equal(await c.incrBy("ctr", -2), 3);
  });
});

test("expire/persist/ttlMs; set_with_ttl atomic", async () => {
  await eachBackend(async (c) => {
    assert.equal(await c.ttlMs("k"), -2, "no key → -2");
    await c.set("k", "v");
    assert.equal(await c.ttlMs("k"), -1, "no TTL → -1");
    assert.equal(await c.expire("k", 60_000), true);
    assert.ok((await c.ttlMs("k")) > 0);
    assert.equal(await c.persist("k"), true);
    assert.equal(await c.ttlMs("k"), -1);
    await c.setWithTtl("t", "x", 60_000);
    assert.ok((await c.ttlMs("t")) > 0);
  });
});

test("typeOf / dbsize / flushall; mget order + nulls; mset atomic", async () => {
  await eachBackend(async (c) => {
    await c.flushall();
    assert.equal(await c.typeOf("nope"), "none");
    await c.set("k", "v");
    assert.equal(await c.typeOf("k"), "string");
    await c.mset("a", "1", "bb", "2");
    const got = (await c.mget("a", "missing", "bb")).map(s);
    assert.deepEqual(got, ["1", null, "2"]);
    assert.ok((await c.dbsize()) >= 3);
    await c.flushall();
    assert.equal(await c.dbsize(), 0);
  });
});

// --- Collections (§6) ---------------------------------------------------

test("hash: hset count, hget/hdel/hlen/hgetall/hkeys/hvals", async () => {
  await eachBackend(async (c) => {
    assert.equal(await c.hset("h", "f1", "v1", "f2", "v2"), 2, "newly added");
    assert.equal(await c.hset("h", "f1", "v1b"), 0, "overwrite not counted");
    assert.equal(s(await c.hget("h", "f1")), "v1b");
    assert.equal(await c.hlen("h"), 2);
    const all = (await c.hgetall("h")).map(s);
    assert.equal(all.length, 4);
    assert.equal((await c.hkeys("h")).length, 2);
    assert.equal((await c.hvals("h")).length, 2);
    assert.equal(await c.hdel("h", "f1"), 1);
  });
});

test("list: push lengths, pop counts, llen, lrange negatives", async () => {
  await eachBackend(async (c) => {
    assert.equal(await c.rpush("l", "a", "b", "c"), 3);
    assert.equal(await c.lpush("l", "z"), 4);
    assert.equal(await c.llen("l"), 4);
    assert.deepEqual((await c.lrange("l", 0, -1)).map(s), ["z", "a", "b", "c"]);
    assert.deepEqual((await c.lpop("l", 2)).map(s), ["z", "a"]);
    assert.deepEqual((await c.rpop("l", 1)).map(s), ["c"]);
  });
});

test("set: sadd/srem counts, smembers, scard, sismember, sinter/sunion/sdiff", async () => {
  await eachBackend(async (c) => {
    assert.equal(await c.sadd("s1", "a", "b", "c"), 3);
    assert.equal(await c.sadd("s2", "b", "c", "d"), 3);
    assert.equal(await c.scard("s1"), 3);
    assert.equal(await c.sismember("s1", "a"), true);
    assert.equal(await c.sismember("s1", "z"), false);
    assert.deepEqual((await c.smembers("s1")).map(s).sort(), ["a", "b", "c"]);
    assert.deepEqual((await c.sinter("s1", "s2")).map(s).sort(), ["b", "c"]);
    assert.deepEqual((await c.sunion("s1", "s2")).map(s).sort(), ["a", "b", "c", "d"]);
    assert.deepEqual((await c.sdiff("s1", "s2")).map(s).sort(), ["a"]);
    assert.equal(await c.srem("s1", "a"), 1);
  });
});

test("zset: zadd, zscore, zcard, zrange asc, zrem", async () => {
  await eachBackend(async (c) => {
    assert.equal(await c.zadd("z", { score: 1, member: "a" }, { score: 3, member: "c" }, { score: 2, member: "b" }), 3);
    assert.equal(await c.zscore("z", "b"), 2);
    assert.equal(await c.zscore("z", "missing"), null);
    assert.equal(await c.zcard("z"), 3);
    assert.deepEqual((await c.zrange("z", 0, -1)).map(s), ["a", "b", "c"]);
    assert.equal(await c.zrem("z", "a"), 1);
  });
});

// --- Sorted-set algebra (§6) --------------------------------------------

test("zinterstore/zunionstore cardinality; WEIGHTS+AGGREGATE; zintercard; empty keys", async () => {
  await eachBackend(async (c) => {
    await c.zadd("za", { score: 1, member: "x" }, { score: 2, member: "y" });
    await c.zadd("zb", { score: 3, member: "y" }, { score: 4, member: "z" });
    assert.equal(await c.zinterstore("dInt", "za", "zb"), 1);
    assert.equal(await c.zunionstore("dUni", "za", "zb"), 3);
    assert.equal(await c.zunionstoreWith("dW", ["za", "zb"], [2, 3], "max"), 3);
    assert.equal(await c.zintercard(["za", "zb"], null), 1);
    assert.equal(await c.zintercard(["za", "zb"], 1), 1);
    await assert.rejects(c.zinterstoreWith("d", [], null, "sum"), (e) => e instanceof InvalidInputError);
  });
});

// --- Hash-field TTL (§6) ------------------------------------------------

test("hexpire (secs) / hpexpire (ms) codes in order; httl/hpttl; hpersist; empty fields", async () => {
  await eachBackend(async (c) => {
    await c.hset("hh", "f1", "v1", "f2", "v2");
    const codes = await c.hpexpire("hh", [b("f1"), b("missing")], 60_000);
    assert.deepEqual(codes, [1, -2], "1 set, -2 missing");
    assert.deepEqual(await c.hexpire("hh", [b("f2")], 60_000), [1]);
    assert.ok((await c.httl("hh", b("f2")))[0]! > 0);
    assert.ok((await c.hpttl("hh", b("f1")))[0]! > 0);
    assert.deepEqual(await c.hpersist("hh", b("f1")), [1]);
    await assert.rejects(c.hexpire("hh", [], 1000), (e) => e instanceof InvalidInputError);
  });
});

// --- Blocking pops (§6) -------------------------------------------------

test("blpop/brpop immediate hit; empty timeout → null; bzpopmin; bad args", async () => {
  await eachBackend(async (c) => {
    await c.rpush("bl", "a", "b");
    const hit = await c.blpop([b("bl")], 1000);
    assert.equal(s(hit!.key), "bl");
    assert.equal(s(hit!.value), "a");
    assert.equal(await c.blpop([b("empty")], 100), null, "timeout → null");
    await c.zadd("bz", { score: 5, member: "lo" }, { score: 9, member: "hi" });
    const z = await c.bzpopmin([b("bz")], 1000);
    assert.equal(s(z!.member), "lo");
    assert.equal(z!.score, 5);
    await assert.rejects(c.blpop([b("k")], 0), (e) => e instanceof InvalidInputError);
    await assert.rejects(c.blpop([], 100), (e) => e instanceof InvalidInputError);
  });
});

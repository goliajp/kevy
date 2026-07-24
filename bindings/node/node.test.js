// The Node smoke — the same contract bun.test.js and typed.test.js pin
// under Bun, phrased in node:test. ffigate runs
// `node --test bindings/node/node.test.js` after `cargo build -p kevy-napi`.
import assert from "node:assert/strict";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { KevyError, open, text } from "./index.js";
import { abi, open as openRaw, openWith, version } from "./node.js";

test("addon smoke", () => {
  assert.equal(abi(), 1);
  assert.match(version(), /^\d+\.\d+\.\d+$/);

  const dir = join(mkdtempSync(join(tmpdir(), "kevy-node-")), "data");
  let db = openRaw({ dir });
  assert.equal(db.cmd("SET", "k", "v"), "OK");
  assert.equal(text(db.cmd("GET", "k")), "v");
  db.close();

  // kill -9 semantics live in the C/Go smokes; here reopen-after-close
  // pins that the data went through the persistence path at all.
  db = openRaw({ dir });
  assert.equal(text(db.cmd("GET", "k")), "v");
  assert.ok(db.cmd("NOSUCHVERB") instanceof KevyError);

  // The boot-replay verdict: this reopen replayed the SET above, and a
  // clean open reports nothing dropped, nothing corrupt.
  const rep = db.openReport();
  assert.ok(rep.replayedCommands >= 1);
  assert.equal(rep.droppedBytes, 0);
  assert.equal(rep.corrupt, false);
  assert.equal(rep.quarantineCount, 0);
  db.close();
});

// The lifecycle passthrough: openWith carries explicit options into the
// engine (shards: 4 round-trips through SET/GET and a reopen), and
// shutdown() is the deterministic teardown — a REAL fsync, then writes are
// refused while reads stay available; reopen sees every pre-shutdown write.
test("shutdown + openWith", () => {
  const dir = join(mkdtempSync(join(tmpdir(), "kevy-node-")), "data");
  let db = openWith(dir, { shards: 4 });
  assert.equal(db.cmd("SET", "k", "v"), "OK");
  db.shutdown();
  db.shutdown(); // idempotent
  assert.throws(() => db.setScalar("late", "x")); // writes refused
  assert.equal(text(db.cmd("GET", "k")), "v"); // reads stay available
  db.close();

  db = openWith(dir, { shards: 4 });
  assert.equal(text(db.cmd("GET", "k")), "v"); // pre-shutdown data survived
  assert.equal(db.cmd("GET", "late"), null); // the refused write does not exist
  db.close();
});

// GET on a non-string key: the scalar shared lane collapses WRONGTYPE to a
// misuse code (throwing the pending N-API exception), so index.js falls back to
// the framed GET; the typed surface must surface the -WRONGTYPE as a thrown
// KevyError whose taxonomy survived the throw. Exercises the new scalar lane +
// its framed fallback end to end.
test("typed GET on a non-string key throws WRONGTYPE", async () => {
  const db = await open(); // in-memory
  assert.equal(db.cmd("RPUSH", "list", "a"), 1); // key now holds a list
  let caught;
  try {
    db.get("list");
  } catch (e) {
    caught = e;
  }
  assert.ok(caught instanceof KevyError);
  assert.ok(caught instanceof Error); // taxonomy + stack both survive the throw
  assert.match(caught.message, /^WRONGTYPE/);
  db.close();
});

test("typed surface", async () => {
  const db = await open(); // in-memory

  db.set("k", "v");
  assert.equal(db.getText("k"), "v");
  assert.equal(db.getText("absent"), undefined);

  db.set("tmp", "x", { ttlMs: 30_000 });
  const ttl = db.pttl("tmp");
  assert.ok(ttl > 0 && ttl <= 30_000);

  assert.equal(db.incrby("hits", 1), 1);
  assert.equal(db.incrby("hits", 10), 11);
  assert.throws(() => db.incrby("k", 1)); // typed layer: WRONGTYPE throws

  const got = db.mget("k", "absent", "tmp");
  assert.equal(text(got[0]), "v");
  assert.equal(got[1], null);
  assert.equal(text(got[2]), "x");

  assert.equal(db.expire("k", 60_000), true);
  assert.deepEqual(db.keys("*").sort(), ["hits", "k", "tmp"]);
  assert.equal(db.dbsize(), 3);
  assert.equal(db.del("k", "absent"), 1);
  db.flushall();
  assert.equal(db.dbsize(), 0);

  // cmd() stays neutral: the protocol error is a VALUE here.
  assert.ok(db.cmd("NOSUCHVERB") instanceof KevyError);

  // callback pub/sub via the pump
  const seen = [];
  db.subscribe("room", (payload, channel) => seen.push([text(payload), channel]));
  assert.equal(db.publish("room", "hi"), 1);
  await new Promise((r) => setTimeout(r, 120)); // > tickMs
  assert.deepEqual(seen, [["hi", "room"]]);

  db.close();
});

test("setMany batches writes, incl. a value past the pack chunk", async () => {
  const db = await open(); // in-memory
  const big = Buffer.alloc(2 << 20, 0x62); // > the ~1 MiB pack chunk
  db.setMany([
    ["a", "1"],
    ["bb", Buffer.alloc(4096, 0x61)],
    ["big", big],
  ]);
  assert.equal(text(db.get("a")), "1");
  assert.equal(db.get("bb").length, 4096);
  assert.equal(db.get("big").length, big.length);
  db.close();
});

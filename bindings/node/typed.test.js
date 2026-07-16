// The typed-surface smoke for @goliapkg/kevy-node (runs under Bun; the same
// file exercises the Node path once the N-API addon lands).
import { expect, test } from "bun:test";

import { KevyError, open, text } from "./index.js";

// GET on a non-string key: the Bun scalar lane collapses WRONGTYPE to a misuse
// code, so index.js falls back to the framed GET; the typed surface must surface
// the -WRONGTYPE as a thrown KevyError whose taxonomy survived the throw.
test("typed GET on a non-string key throws WRONGTYPE", async () => {
  const db = await open();
  expect(db.cmd("RPUSH", "list", "a")).toBe(1); // key now holds a list
  let caught;
  try {
    db.get("list");
  } catch (e) {
    caught = e;
  }
  expect(caught).toBeInstanceOf(KevyError);
  expect(caught).toBeInstanceOf(Error); // taxonomy + stack both survive the throw
  expect(caught.message).toMatch(/^WRONGTYPE/);
  db.close();
});

test("typed surface", async () => {
  const db = await open(); // in-memory

  db.set("k", "v");
  expect(db.getText("k")).toBe("v");
  expect(db.getText("absent")).toBeUndefined();

  db.set("tmp", "x", { ttlMs: 30_000 });
  const ttl = db.pttl("tmp");
  expect(ttl).toBeGreaterThan(0);
  expect(ttl).toBeLessThanOrEqual(30_000);

  expect(db.incrby("hits", 1)).toBe(1);
  expect(db.incrby("hits", 10)).toBe(11);
  expect(() => db.incrby("k", 1)).toThrow(); // typed layer: WRONGTYPE throws

  const got = db.mget("k", "absent", "tmp");
  expect(text(got[0])).toBe("v");
  expect(got[1]).toBeNull();
  expect(text(got[2])).toBe("x");

  expect(db.expire("k", 60_000)).toBe(true);
  expect(db.keys("*").sort()).toEqual(["hits", "k", "tmp"]);
  expect(db.dbsize()).toBe(3);
  expect(db.del("k", "absent")).toBe(1);
  db.flushall();
  expect(db.dbsize()).toBe(0);

  // cmd() stays neutral: the protocol error is a VALUE here.
  expect(db.cmd("NOSUCHVERB")).toBeInstanceOf(KevyError);

  // callback pub/sub via the pump
  const seen = [];
  db.subscribe("room", (payload, channel) => seen.push([text(payload), channel]));
  expect(db.publish("room", "hi")).toBe(1);
  await new Promise((r) => setTimeout(r, 120)); // > tickMs
  expect(seen).toEqual([["hi", "room"]]);

  db.close();
});

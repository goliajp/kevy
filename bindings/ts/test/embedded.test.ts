// Conformance: the kevy-embedded store contract (§5.2 / §6 "Embedded store
// contract"). open/openMem/close, cmd(argv) runs an arbitrary verb, scalar
// get/set fast paths, subscribe → poll next() + blocking wait(), and
// persistence survives a close/reopen. Runs on node --test and bun test.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { EmbeddedDb, replyText, textOf, version } from "../src/index.ts";

test("openMem + cmd runs arbitrary verbs; -ERR is data", () => {
  const db = EmbeddedDb.openMem();
  try {
    assert.equal(replyText(db.cmd("SET", "k", "v1")), "OK");
    assert.equal(replyText(db.cmd("GET", "k")), "v1");
    const bad = db.cmd("NOSUCHVERB");
    assert.equal(bad.kind, "error");
    const n = db.cmd("DEL", "k");
    assert.deepEqual(n, { kind: "int", value: 1 });
  } finally {
    db.close();
  }
});

test("scalar get/set fast paths (with ttl)", () => {
  const db = EmbeddedDb.openMem();
  try {
    db.setScalar("s", "hello");
    assert.equal(textOf(db.getScalar("s")!), "hello");
    assert.equal(db.getScalar("missing"), null);
    db.setScalar("t", "temp", 60_000);
    assert.equal(textOf(db.getScalar("t")!), "temp");
  } finally {
    db.close();
  }
});

test("subscribe → poll next() + PUBLISH delivers a message frame", () => {
  const db = EmbeddedDb.openMem();
  try {
    const sub = db.subscribe("c1");
    const ack = sub.next();
    assert.ok(ack && ack.kind === "array", "subscribe ack");
    assert.deepEqual(db.cmd("PUBLISH", "c1", "hello"), { kind: "int", value: 1 });
    const frame = sub.wait(1000);
    assert.ok(frame && frame.kind === "array");
    if (frame && frame.kind === "array") {
      assert.equal(replyText(frame.items[0]!), "message");
      assert.equal(replyText(frame.items[1]!), "c1");
      assert.equal(replyText(frame.items[2]!), "hello");
    }
    assert.equal(sub.next(), null, "queue drained");
    sub.close();
  } finally {
    db.close();
  }
});

test("persistence: write, close, reopen same dir → state survives", () => {
  const dir = mkdtempSync(join(tmpdir(), "kevy-ts-"));
  try {
    let db = EmbeddedDb.open(dir);
    assert.equal(replyText(db.cmd("SET", "persist:k", "durable")), "OK");
    db.close();
    db = EmbeddedDb.open(dir);
    assert.equal(replyText(db.cmd("GET", "persist:k")), "durable");
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("version is non-empty", () => {
  assert.ok(version().length > 0);
});

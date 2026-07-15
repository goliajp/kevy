// Conformance: the RESP2/3 codec (contract §4.1) + error classification
// (§2.2). Runs identically on `node --test` and `bun test`.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  decodeReply,
  parseReply,
  encodeCommand,
  replyText,
  isNil,
  isError,
  toBytes,
  type Reply,
} from "../src/index.ts";
import { classifyStoreError } from "../src/errors.ts";

const bytes = (s: string) => new TextEncoder().encode(s);
const dec = (s: string): Reply => decodeReply(bytes(s));

test("resp2 scalars", () => {
  assert.equal(replyText(dec("+OK\r\n")), "OK");
  const e = dec("-WRONGTYPE nope\r\n");
  assert.ok(isError(e));
  assert.equal(replyText(e), "WRONGTYPE nope");
  const i = dec(":42\r\n");
  assert.deepEqual(i, { kind: "int", value: 42 });
  assert.equal(replyText(dec("$3\r\nabc\r\n")), "abc");
  assert.ok(isNil(dec("$-1\r\n")));
  assert.ok(isNil(dec("*-1\r\n")));
});

test("resp2 array with mixed elements", () => {
  const r = dec("*3\r\n$1\r\na\r\n:7\r\n$-1\r\n");
  assert.equal(r.kind, "array");
  if (r.kind !== "array") return;
  assert.equal(replyText(r.items[0]!), "a");
  assert.deepEqual(r.items[1], { kind: "int", value: 7 });
  assert.ok(isNil(r.items[2]!));
});

test("resp3 shapes: map, set, double, boolean, verbatim, bignum, null, push", () => {
  const m = dec("%1\r\n$1\r\nk\r\n$1\r\nv\r\n");
  assert.equal(m.kind, "map");
  assert.equal(dec("~2\r\n:1\r\n:2\r\n").kind, "set");
  assert.deepEqual(dec(",3.14\r\n"), { kind: "double", value: 3.14 });
  assert.deepEqual(dec(",inf\r\n"), { kind: "double", value: Infinity });
  assert.deepEqual(dec("#t\r\n"), { kind: "boolean", value: true });
  const vb = dec("=15\r\ntxt:hello world\r\n");
  assert.equal(vb.kind, "verbatim");
  if (vb.kind === "verbatim") {
    assert.equal(vb.format, "txt");
    assert.equal(replyText(vb), "hello world");
  }
  assert.equal(dec("(3492890328409238509324850943850943825024385\r\n").kind, "bigNumber");
  assert.deepEqual(dec("_\r\n"), { kind: "null" });
  assert.equal(dec(">3\r\n$7\r\nmessage\r\n$2\r\nch\r\n$2\r\nhi\r\n").kind, "push");
});

test("attribute frames are transparently skipped", () => {
  const r = dec("|1\r\n$3\r\nkey\r\n$3\r\nval\r\n:5\r\n");
  assert.deepEqual(r, { kind: "int", value: 5 });
});

test("incremental parse: partial frame needs more", () => {
  const full = bytes("$5\r\nhello\r\n");
  const [, used0] = parseReply(full.subarray(0, 6), 0); // "$5\r\nhe"
  assert.equal(used0, 0, "partial frame yields need-more");
  const [r, used] = parseReply(full, 0);
  assert.equal(used, full.length);
  assert.equal(replyText(r), "hello");
});

test("encodeCommand produces RESP multibulk", () => {
  const wire = encodeCommand([toBytes("SET"), toBytes("k"), toBytes("v")]);
  assert.equal(new TextDecoder().decode(wire), "*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
});

test("encode/decode round-trips binary-safe payloads", () => {
  const raw = new Uint8Array([0, 1, 2, 255, 13, 10, 0]);
  const framed = bytes(`$${raw.length}\r\n`);
  const buf = new Uint8Array(framed.length + raw.length + 2);
  buf.set(framed);
  buf.set(raw, framed.length);
  buf.set([13, 10], framed.length + raw.length);
  const r = decodeReply(buf);
  assert.equal(r.kind, "bulk");
  if (r.kind === "bulk") assert.deepEqual([...r.bytes], [...raw]);
});

test("classifyStoreError recognizes store-semantic errors", () => {
  assert.equal(classifyStoreError("WRONGTYPE Operation against ..."), "wrongType");
  assert.equal(classifyStoreError("value is not an integer or out of range"), "notInteger");
  assert.equal(classifyStoreError("value is not a valid float"), "notFloat");
  assert.equal(classifyStoreError("no such key"), "noSuchKey");
  assert.equal(classifyStoreError("OOM command not allowed"), "outOfMemory");
  assert.equal(classifyStoreError("ERR wrong number of arguments"), null);
});

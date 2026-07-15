// Change feed conformance (contract §3.10 / §6). Remote requires the server
// started with [feed] enabled; embedded requires feed-enabled config (this
// port's embedded connect does not), so embedded feed_tail/read → Unsupported
// and feed_shards is 1. node --test + bun test.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { connect, InvalidInputError, ProtocolError, UnsupportedError } from "../src/index.ts";
import { spawnServerConfig, uniqueMemUrl, s, type Server } from "./harness.ts";

let server: Server;
before(async () => {
  server = await spawnServerConfig("[feed]\nenabled = true\n", ["--threads", "1"]);
});
after(() => server?.close());

test("feed_shards / feed_tail / feed_read with frames + resume", async () => {
  const c = await connect(server.url);
  assert.ok((await c.feedShards()) >= 1);
  const tail = await c.feedTail(0);

  await c.set("fk1", "v1");
  await c.set("fk2", "v2");

  const batch = await c.feedRead(0, tail.generation, tail.nextOffset, null);
  assert.ok(batch.frames.length >= 2, "expected >= 2 frames");
  assert.ok(
    batch.frames.some((f) => f.argv.length > 0 && s(f.argv[0]!) === "SET"),
    "a SET frame is present",
  );

  const next = await c.feedRead(0, batch.generation, batch.nextOffset, null);
  assert.equal(next.frames.length, 0, "resume from tail is caught up");
  c.close();
});

test("stale cursor → Protocol (FEEDRESYNC) error", async () => {
  const c = await connect(server.url);
  await assert.rejects(
    c.feedRead(0, 999999, 0, null),
    (e) => e instanceof ProtocolError,
  );
  c.close();
});

test("embedded feed: shards=1, non-zero shard InvalidInput, disabled Unsupported", async () => {
  const c = await connect(uniqueMemUrl());
  assert.equal(await c.feedShards(), 1);
  await assert.rejects(c.feedTail(1), (e) => e instanceof InvalidInputError);
  await assert.rejects(c.feedTail(0), (e) => e instanceof UnsupportedError);
  c.close();
});

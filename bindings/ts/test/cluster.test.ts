// Cluster CRC16 routing conformance (contract §3.15 / §6). Requires the
// server in cluster mode (shards bind port+1..+N); correct routing means
// -MOVED never fires. Also checks CRC16 against the known vector. node + bun.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { ClusterClient, crc16, keyHashSlot } from "../src/index.ts";
import { spawnServer, b, s, type Server } from "./harness.ts";

test("crc16 check vector + hashtag", () => {
  assert.equal(crc16(b("123456789")), 0x31c3);
  // {hashtag} routes by the inner span only.
  assert.equal(keyHashSlot(b("{user}.a")), keyHashSlot(b("{user}.b")));
});

let server: Server;
before(async () => {
  server = await spawnServer(["--cluster", "--threads", "4"]);
});
after(() => server?.close());

test("CLUSTER SLOTS topology + CRC16 routing; del/exists sum; dbsize/flushall", async () => {
  const cc = await ClusterClient.connect("127.0.0.1", server.port);
  try {
    assert.equal(cc.shardCount, 4, "four shards");

    const keys = ["k0", "k1", "user:42", "rate:10.0.0.1", "gl:abc", "alpha", "beta", "gamma"];
    for (let i = 0; i < keys.length; i++) {
      await cc.set(keys[i]!, "v" + i); // routes to owner shard; -MOVED would throw
      assert.equal(s(await cc.get(keys[i]!)), "v" + i);
    }

    assert.ok((await cc.incr("counter")) >= 1);
    await cc.ping();

    assert.equal(await cc.del("k0", "k1", "user:42", "absent"), 3, "cross-shard del sum");
    assert.equal(await cc.exists("alpha", "beta", "gamma"), 3, "cross-shard exists sum");

    assert.ok((await cc.dbsize()) >= 1, "whole-cluster dbsize");
    await cc.flushall();
  } finally {
    cc.close();
  }
});

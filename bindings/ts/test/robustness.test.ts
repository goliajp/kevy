// Reconnect / robustness (contract §6): a dropped remote connection surfaces
// Closed/Io, and the client can resume on a fresh connect. node + bun.

import { test } from "node:test";
import assert from "node:assert/strict";
import { connect, KevyError } from "../src/index.ts";
import { spawnServer, s } from "./harness.ts";

test("dropped remote connection → Closed/Io; fresh connect resumes", async () => {
  const server = await spawnServer();
  const c = await connect(server.url);
  await c.set("k", "v1");
  assert.equal(s(await c.get("k")), "v1");

  // Kill the server under the connection.
  server.close();
  await assert.rejects(
    (async () => {
      // A few attempts: the failure surfaces as a Closed/Io KevyError.
      for (let i = 0; i < 20; i++) await c.get("k");
    })(),
    (e) => e instanceof KevyError && (e.kind === "closed" || e.kind === "io"),
  );
  c.close();

  // A fresh connect to a new server resumes normal operation.
  const server2 = await spawnServer();
  try {
    const c2 = await connect(server2.url);
    await c2.set("k", "v2");
    assert.equal(s(await c2.get("k")), "v2");
    c2.close();
  } finally {
    server2.close();
  }
});

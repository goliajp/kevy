// The main-process bridge, tested headlessly: a REAL in-memory kevy engine
// (the Node door) behind a fake ipcMain — no BrowserWindow, no display. This
// is the whole point of the DI in bridge.js. Run: `node --test test/*.test.js`
// (ffigate builds kevy-napi first so the Node door's addon loads).
import assert from "node:assert/strict";
import { test } from "node:test";

import { open } from "../../node/index.js";
import { registerKevyHandlers } from "../bridge.js";
import { CHANNELS, decodeReply, KevyError } from "../wire.js";

/** A fake ipcMain that captures each channel's handler for direct calling. */
function fakeIpcMain() {
  const handlers = new Map();
  return {
    handle: (channel, fn) => handlers.set(channel, fn),
    removeHandler: (channel) => handlers.delete(channel),
    call: (channel, event, ...args) => handlers.get(channel)(event, ...args),
    has: (channel) => handlers.has(channel),
  };
}

/** A fake webContents (IPC sender) collecting what the pump pushes to it. */
function fakeSender(id = 1) {
  const sent = [];
  let destroyed = false;
  return {
    id,
    sent,
    send: (channel, payload) => sent.push([channel, payload]),
    isDestroyed: () => destroyed,
    once: () => {},
    kill: () => {
      destroyed = true;
    },
  };
}

test("request/response verbs over the bridge", async () => {
  const db = await open(); // in-memory
  const ipc = fakeIpcMain();
  const bridge = registerKevyHandlers({ ipcMain: ipc, db, version: "4.0.0" });
  const ev = { sender: fakeSender() };

  assert.equal(ipc.call(CHANNELS.version, ev), "4.0.0");

  // cmd() is the neutral escape hatch: OK, then a bytes GET, then a
  // protocol error as a KevyError VALUE (encoded → decoded back).
  assert.equal(ipc.call(CHANNELS.cmd, ev, ["SET", "k", "v"]), "OK");
  assert.equal(new TextDecoder().decode(ipc.call(CHANNELS.cmd, ev, ["GET", "k"])), "v");
  assert.ok(decodeReply(ipc.call(CHANNELS.cmd, ev, ["NOSUCHVERB"])) instanceof KevyError);

  // typed verbs
  assert.equal(new TextDecoder().decode(ipc.call(CHANNELS.get, ev, "k")), "v");
  ipc.call(CHANNELS.set, ev, "n", "1", {});
  assert.equal(ipc.call(CHANNELS.incrby, ev, "n", 10), 11);
  assert.equal(ipc.call(CHANNELS.ttl, ev, "k"), -1); // exists, no TTL
  ipc.call(CHANNELS.set, ev, "t", "x", { ttlMs: 30_000 });
  assert.ok(ipc.call(CHANNELS.ttl, ev, "t") > 0);
  const got = ipc.call(CHANNELS.mget, ev, ["k", "absent"]);
  assert.equal(new TextDecoder().decode(got[0]), "v");
  assert.equal(got[1], null);
  assert.equal(ipc.call(CHANNELS.del, ev, ["k", "absent"]), 1);

  // a typed verb on the wrong type THROWS (→ invoke() rejects in the renderer)
  ipc.call(CHANNELS.cmd, ev, ["LPUSH", "list", "a"]);
  assert.throws(() => ipc.call(CHANNELS.incrby, ev, "list", 1));

  bridge.dispose();
  assert.equal(ipc.has(CHANNELS.cmd), false); // handlers removed on dispose
  db.close();
});

test("pub/sub streams frames to the subscribing renderer", async () => {
  const db = await open();
  const ipc = fakeIpcMain();
  const bridge = registerKevyHandlers({ ipcMain: ipc, db });
  const sender = fakeSender(7);
  const ev = { sender };

  ipc.call(CHANNELS.subscribe, ev, { subId: "s1", channel: "room", pattern: false });
  assert.equal(ipc.call(CHANNELS.publish, ev, "room", "hi"), 1);
  bridge.pump();

  // The pump forwards every frame (subscribe ack + the delivery) on the
  // per-subscription event channel; find the message among them.
  const evtChannel = CHANNELS.event + "s1";
  const frames = sender.sent.filter(([ch]) => ch === evtChannel).map(([, f]) => decodeReply(f));
  const msg = frames.find((f) => Array.isArray(f) && new TextDecoder().decode(f[0]) === "message");
  assert.ok(msg, "a message frame reached the renderer");
  assert.equal(new TextDecoder().decode(msg[1]), "room"); // channel
  assert.equal(new TextDecoder().decode(msg[2]), "hi"); // payload

  // unsubscribe → a later publish delivers nothing more
  ipc.call(CHANNELS.unsubscribe, ev, "s1");
  sender.sent.length = 0;
  ipc.call(CHANNELS.publish, ev, "room", "again");
  bridge.pump();
  assert.equal(sender.sent.length, 0);

  bridge.dispose();
  db.close();
});

test("a destroyed renderer's subscriptions are reclaimed by the pump", async () => {
  const db = await open();
  const ipc = fakeIpcMain();
  const bridge = registerKevyHandlers({ ipcMain: ipc, db });
  const sender = fakeSender(9);

  ipc.call(CHANNELS.subscribe, { sender }, { subId: "s1", channel: "room", pattern: false });
  sender.kill(); // renderer window gone
  ipc.call(CHANNELS.publish, { sender }, "room", "hi");
  bridge.pump(); // must not throw and must not push to the dead sender
  assert.equal(sender.sent.length, 0);

  bridge.dispose();
  db.close();
});

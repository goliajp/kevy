// The preload's renderer API, tested headlessly with a fake ipcRenderer:
// the shape it exposes, that each method routes to the right channel/args,
// and that a pushed pub/sub frame reaches the caller's callback. No Electron.
import assert from "node:assert/strict";
import { test } from "node:test";

import { makeKevyApi, CHANNELS, KevyError } from "../preload.cjs";
import { encodeReply } from "../wire.js";

/** A fake ipcRenderer: records invoke() calls (returning a canned reply) and
 *  lets a test emit an event to a registered on() listener. */
function fakeIpcRenderer() {
  const invokes = [];
  const listeners = new Map();
  let nextReply;
  return {
    invoke: (channel, ...args) => {
      invokes.push({ channel, args });
      return Promise.resolve(nextReply);
    },
    on: (channel, fn) => listeners.set(channel, fn),
    removeListener: (channel) => listeners.delete(channel),
    // test helpers
    invokes,
    reply: (v) => (nextReply = v),
    emit: (channel, payload) => listeners.get(channel)?.(null, payload),
    listenerChannels: () => [...listeners.keys()],
  };
}

test("the exposed API has the full renderer shape", () => {
  const api = makeKevyApi(fakeIpcRenderer());
  const expected = [
    "cmd", "get", "getText", "set", "del", "incrby", "expire",
    "ttl", "mget", "publish", "version", "subscribe", "psubscribe",
  ];
  for (const name of expected) assert.equal(typeof api[name], "function", name);
});

test("typed calls route to the right channel and args", async () => {
  const ipc = fakeIpcRenderer();
  const api = makeKevyApi(ipc);

  ipc.reply(undefined);
  await api.set("k", "v", { ttlMs: 1000 });
  await api.del("a", "b");
  ipc.reply(new Uint8Array([118]));
  assert.equal(await api.getText("k"), "v"); // 0x76 = "v"

  assert.deepEqual(ipc.invokes[0], { channel: CHANNELS.set, args: ["k", "v", { ttlMs: 1000 }] });
  assert.deepEqual(ipc.invokes[1], { channel: CHANNELS.del, args: [["a", "b"]] });
  assert.deepEqual(ipc.invokes[2], { channel: CHANNELS.get, args: ["k"] });
});

test("cmd() decodes a protocol error back into a KevyError value", async () => {
  const ipc = fakeIpcRenderer();
  const api = makeKevyApi(ipc);
  ipc.reply(encodeReply(new KevyError("ERR unknown command 'NOSUCHVERB'")));
  const out = await api.cmd("NOSUCHVERB");
  assert.ok(out instanceof KevyError);
  assert.match(out.message, /NOSUCHVERB/);
  assert.deepEqual(ipc.invokes[0], { channel: CHANNELS.cmd, args: [["NOSUCHVERB"]] });
});

test("subscribe wires an event listener and delivers frames to the callback", async () => {
  const ipc = fakeIpcRenderer();
  const api = makeKevyApi(ipc);
  ipc.reply(undefined);

  const seen = [];
  const unsubscribe = await api.subscribe("room", (payload, channel) =>
    seen.push([new TextDecoder().decode(payload), channel]),
  );

  // subscribe() invoked the subscribe channel with a subId + channel/pattern
  const sub = ipc.invokes.find((i) => i.channel === CHANNELS.subscribe);
  assert.ok(sub, "subscribe channel invoked");
  assert.equal(sub.args[0].channel, "room");
  assert.equal(sub.args[0].pattern, false);
  const subId = sub.args[0].subId;

  // the main process pushes a delivery frame on the event channel
  const enc = (s) => new TextEncoder().encode(s);
  ipc.emit(CHANNELS.event + subId, encodeReply([enc("message"), enc("room"), enc("hi")]));
  assert.deepEqual(seen, [["hi", "room"]]);

  // an ack frame (kind "subscribe") is dropped, not delivered
  ipc.emit(CHANNELS.event + subId, encodeReply([enc("subscribe"), enc("room"), 1]));
  assert.equal(seen.length, 1);

  await unsubscribe();
  assert.equal(ipc.listenerChannels().includes(CHANNELS.event + subId), false);
  assert.ok(ipc.invokes.some((i) => i.channel === CHANNELS.unsubscribe && i.args[0] === subId));
});

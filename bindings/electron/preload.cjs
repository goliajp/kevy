// The preload — the ONLY file a renderer loads. It runs in the isolated
// preload world and exposes an async `window.kevy` over contextBridge; the
// engine itself never enters the renderer. Safe under contextIsolation:true
// and sandbox:true (Electron's default since 20/12).
//
// It is deliberately ONE self-contained CommonJS file with no local require:
// a sandboxed renderer's polyfilled require() can only reach `electron` and a
// few Node built-ins, never a sibling module. So CHANNELS + the KevyError
// marker + decodeReply are inlined here — wire.test.js pins them against the
// canonical copy in wire.js.
//
// Point a window at it:
//   new BrowserWindow({ webPreferences: {
//     preload: require.resolve("@goliapkg/kevy-electron/preload"),
//     contextIsolation: true, sandbox: true } });

"use strict";

// ── inlined wire contract (keep in sync with wire.js; guarded by a test) ──
class KevyError {
  constructor(message) {
    this.message = message;
  }
}
const CHANNELS = {
  cmd: "kevy:req:cmd",
  get: "kevy:req:get",
  set: "kevy:req:set",
  del: "kevy:req:del",
  incrby: "kevy:req:incrby",
  expire: "kevy:req:expire",
  ttl: "kevy:req:ttl",
  mget: "kevy:req:mget",
  publish: "kevy:req:publish",
  version: "kevy:req:version",
  subscribe: "kevy:sub",
  unsubscribe: "kevy:unsub",
  event: "kevy:evt:",
};
function decodeReply(v) {
  if (
    v !== null &&
    typeof v === "object" &&
    !Array.isArray(v) &&
    !(v instanceof Uint8Array) &&
    "__kevyError" in v
  ) {
    return new KevyError(v.__kevyError);
  }
  if (Array.isArray(v)) return v.map(decodeReply);
  return v;
}

const utf8 = (v) => (v instanceof Uint8Array ? new TextDecoder().decode(v) : v);

/**
 * Build the `window.kevy` API over an injected ipcRenderer. Exported so the
 * shape and routing are unit-tested (test/preload.test.js) with a fake
 * ipcRenderer — no Electron runtime, no window.
 */
function makeKevyApi(ipcRenderer) {
  const invoke = (channel, ...args) => ipcRenderer.invoke(channel, ...args);
  let subCounter = 0;

  function subscribeImpl(name, pattern, cb) {
    const subId = `s${++subCounter}`;
    const listener = (_event, encoded) => deliver(decodeReply(encoded), pattern, cb);
    ipcRenderer.on(CHANNELS.event + subId, listener);
    return invoke(CHANNELS.subscribe, { subId, channel: name, pattern }).then(() => async () => {
      ipcRenderer.removeListener(CHANNELS.event + subId, listener);
      await invoke(CHANNELS.unsubscribe, subId);
    });
  }

  return {
    // ── escape hatch: every verb, a protocol error as a KevyError VALUE ──
    cmd: (...argv) => invoke(CHANNELS.cmd, argv).then(decodeReply),

    // ── typed surface (mirrors the Node/wasm doors); errors throw ────────
    get: (key) => invoke(CHANNELS.get, key),
    getText: (key) => invoke(CHANNELS.get, key).then((v) => (v == null ? undefined : utf8(v))),
    set: (key, value, opts) => invoke(CHANNELS.set, key, value, opts),
    del: (...keys) => invoke(CHANNELS.del, keys),
    incrby: (key, delta = 1) => invoke(CHANNELS.incrby, key, delta),
    expire: (key, ttlMs) => invoke(CHANNELS.expire, key, ttlMs),
    ttl: (key) => invoke(CHANNELS.ttl, key),
    mget: (...keys) => invoke(CHANNELS.mget, keys),
    publish: (channel, payload) => invoke(CHANNELS.publish, channel, payload),
    version: () => invoke(CHANNELS.version),

    // ── live pub/sub: resolves to an unsubscribe function ────────────────
    subscribe: (channel, cb) => subscribeImpl(channel, false, cb),
    psubscribe: (pattern, cb) => subscribeImpl(pattern, true, cb),
  };
}

/** Route one decoded frame to the caller's callback (ack frames are dropped,
 *  matching the Node door's pump; deliveries carry payload + channel). */
function deliver(frame, pattern, cb) {
  if (!Array.isArray(frame)) return;
  const kind = utf8(frame[0]);
  if (!pattern && kind === "message") cb(frame[2], utf8(frame[1]));
  else if (pattern && kind === "pmessage") cb(frame[3], utf8(frame[2]), utf8(frame[1]));
}

// ── real wiring, only inside Electron's renderer (skipped under `node --test`) ──
if (process.versions && process.versions.electron) {
  const { contextBridge, ipcRenderer } = require("electron");
  const api = makeKevyApi(ipcRenderer);
  if (process.contextIsolated) contextBridge.exposeInMainWorld("kevy", api);
  else globalThis.kevy = api; // isolation off: attach to the page global (per Electron security guide)
}

module.exports = { makeKevyApi, CHANNELS, decodeReply, KevyError };

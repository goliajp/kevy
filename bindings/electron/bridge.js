// The main-process bridge: it owns nothing itself but wires an already-open
// kevy store (the Node door's typed Db — see main.js) onto an injected
// ipcMain. Everything here is pure over its { ipcMain, db } dependencies, so
// the whole surface is exercised headlessly in test/bridge.test.js with a
// fake ipcMain and a real in-memory engine — no BrowserWindow, no display.
//
// Two IPC shapes:
//   • request/response verbs  — ipcMain.handle; a typed method that throws
//     (WRONGTYPE, misuse) rejects the renderer's invoke() promise, matching
//     the TS client's throw-on-protocol-error idiom (client-contract §2.1).
//     cmd() is the neutral escape hatch: a protocol error comes back as a
//     KevyError VALUE (encodeReply), not a rejection.
//   • pub/sub — a single unref'd pump drains every live subscription and
//     pushes each frame to the owning renderer's per-subscription event
//     channel. Polled, never callback-across-FFI (kevy.h's discipline).

import { CHANNELS, encodeReply } from "./wire.js";

/** Request/response verbs, mapped to the Node door's typed Db. cmd() alone
 *  returns errors-as-values; the typed verbs throw (→ invoke rejects). */
function commandHandlers(db, version) {
  return {
    [CHANNELS.cmd]: (argv) => encodeReply(db.cmd(...argv)),
    [CHANNELS.get]: (key) => db.get(key),
    [CHANNELS.set]: (key, value, opts) => db.set(key, value, opts ?? {}),
    [CHANNELS.del]: (keys) => db.del(...keys),
    [CHANNELS.incrby]: (key, delta) => db.incrby(key, delta),
    [CHANNELS.expire]: (key, ttlMs) => db.expire(key, ttlMs),
    [CHANNELS.ttl]: (key) => db.pttl(key),
    [CHANNELS.mget]: (keys) => db.mget(...keys),
    [CHANNELS.publish]: (channel, payload) => db.publish(channel, payload),
    [CHANNELS.version]: () => version,
  };
}

/** Open one raw subscription for a renderer, tracked so the pump can push to
 *  it and dispose/destroy can close it. Keyed by (webContents.id, subId). */
function openSub(state, db, sender, { subId, channel, pattern }) {
  const raw = pattern ? db.psubscribeRaw(channel) : db.subscribeRaw(channel);
  const forWc = state.subs.get(sender.id) ?? new Map();
  forWc.set(subId, { raw, sender });
  state.subs.set(sender.id, forWc);
  if (!state.watched.has(sender.id)) {
    state.watched.add(sender.id);
    sender.once("destroyed", () => closeAll(state, sender.id));
  }
}

/** Close one subscription (renderer asked) or all of a gone renderer's. */
function closeOne(state, wcId, subId) {
  const forWc = state.subs.get(wcId);
  const entry = forWc?.get(subId);
  if (!entry) return;
  entry.raw.close();
  forWc.delete(subId);
  if (forWc.size === 0) state.subs.delete(wcId);
}

function closeAll(state, wcId) {
  const forWc = state.subs.get(wcId);
  if (!forWc) return;
  for (const { raw } of forWc.values()) raw.close();
  state.subs.delete(wcId);
}

/** Drain every live subscription once, pushing frames to their renderer.
 *  A gone renderer's frames are dropped and its subs reclaimed. */
function pumpOnce(state) {
  for (const [wcId, forWc] of state.subs) {
    for (const [subId, { raw, sender }] of forWc) {
      if (sender.isDestroyed()) {
        closeOne(state, wcId, subId);
        continue;
      }
      for (let f = raw.next(); f !== undefined; f = raw.next()) {
        sender.send(CHANNELS.event + subId, encodeReply(f));
      }
    }
  }
}

/**
 * Register every kevy IPC handler on `ipcMain`, backed by the open store
 * `db`. `tickMs` paces the pub/sub pump (default 50). Returns a handle with
 * an explicit `pump()` (tests drive it deterministically) and `dispose()`.
 */
export function registerKevyHandlers({ ipcMain, db, tickMs = 50, version = "" }) {
  const handlers = commandHandlers(db, version);
  for (const [channel, fn] of Object.entries(handlers)) {
    ipcMain.handle(channel, (_event, ...args) => fn(...args));
  }

  const state = { subs: new Map(), watched: new Set() };
  ipcMain.handle(CHANNELS.subscribe, (event, req) => openSub(state, db, event.sender, req));
  ipcMain.handle(CHANNELS.unsubscribe, (event, subId) => closeOne(state, event.sender.id, subId));

  const pump = () => pumpOnce(state);
  const timer = setInterval(pump, tickMs);
  timer.unref?.();

  const channels = [...Object.keys(handlers), CHANNELS.subscribe, CHANNELS.unsubscribe];
  return {
    pump,
    dispose() {
      clearInterval(timer);
      for (const wcId of [...state.subs.keys()]) closeAll(state, wcId);
      for (const channel of channels) ipcMain.removeHandler?.(channel);
    },
  };
}

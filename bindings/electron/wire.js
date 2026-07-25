// The IPC wire contract shared by the main-process bridge (bridge.js) and
// the tests. The preload (preload.cjs) MUST stay a single self-contained
// file — a sandboxed renderer's polyfilled require() cannot load a local
// module — so it inlines a byte-for-byte copy of CHANNELS + the KevyError
// marker + decodeReply. wire.test.js pins the two copies in sync.
//
// Electron IPC moves values by the structured-clone algorithm: strings,
// numbers, bigints, Uint8Array, null and nested arrays all survive as-is.
// The one reply shape that does NOT is a KevyError instance (a class, so a
// clone would drop its identity), so encodeReply rewrites it to a plain
// { __kevyError } marker and the far side reconstructs it.

/** A protocol error returned as a VALUE by cmd() (mirrors the Node door's
 *  resp.js: the engine saying no is data; a throw is reserved for misuse). */
export class KevyError {
  constructor(message) {
    this.message = message;
  }
}

/** IPC channel names. Request/response verbs are one channel each; a live
 *  subscription streams over a per-subscription event channel. */
export const CHANNELS = {
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
  /** Per-subscription push channel: this prefix + the renderer's subId. */
  event: "kevy:evt:",
};

// The reply domain over the Node door (a RESP2 parser) is:
//   string | number | bigint | Uint8Array | null | Array | KevyError.
// So any non-null, non-array, non-bytes OBJECT is a protocol-error value —
// the Node door's resp.js has its OWN KevyError class, distinct from ours, so
// we detect by shape, not instanceof (that class boundary is why we must).
const isErrorValue = (v) =>
  v !== null && typeof v === "object" && !Array.isArray(v) && !(v instanceof Uint8Array);

/** Rewrite a decoded reply into a structured-clone-safe value: protocol-error
 *  values (possibly nested inside arrays) become { __kevyError } markers. */
export function encodeReply(v) {
  if (isErrorValue(v)) return { __kevyError: v.message };
  if (Array.isArray(v)) return v.map(encodeReply);
  return v;
}

/** The inverse of encodeReply: markers become KevyError instances again. */
export function decodeReply(v) {
  if (isErrorValue(v) && "__kevyError" in v) return new KevyError(v.__kevyError);
  if (Array.isArray(v)) return v.map(decodeReply);
  return v;
}

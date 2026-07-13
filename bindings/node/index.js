// @goliajp/kevy-node — kevy embedded, one API on two runtimes.
//
// Bun loads the engine over bun:ffi; Node loads the hand-written N-API
// addon (kevy-napi). Either way this file is the API: the typed methods below
// mirror @goliajp/kevy (the wasm package), and cmd() reaches all 184 verbs.
//
//   import { open } from "@goliajp/kevy-node";
//   const db = open({ dir: "data/" });
//   db.set("session:7f3a", value, { ttlMs: 3_600_000 });
//   db.getText("session:7f3a");
//
// Typed methods treat a protocol error as a THROWN error — a typed call
// has one meaning, so "-WRONGTYPE" is a failure. cmd() returns KevyError
// as a value for callers driving the raw verb surface.

import { KevyError, text } from "./resp.js";

export { KevyError, text };

const isBun = typeof globalThis.Bun !== "undefined";

let backend;
async function loadBackend() {
  if (!backend) {
    backend = await import(isBun ? "./bun.js" : "./node.js");
  }
  return backend;
}

function reject(v) {
  if (v instanceof KevyError) throw new Error(`kevy: ${v.message}`);
  return v;
}

class Db {
  #raw;
  #subs = [];
  #timer;
  #tickMs;

  constructor(raw, tickMs) {
    this.#raw = raw;
    this.#tickMs = tickMs;
  }

  // ── the escape hatch: every verb, RESP semantics, errors as values ──
  cmd(...argv) {
    return this.#raw.cmd(...argv);
  }

  // ── the typed surface (mirrors the wasm package) ────────────────────
  set(key, value, opts = {}) {
    if (opts.ttlMs != null) {
      reject(this.#raw.cmd("SET", key, value, "PX", String(opts.ttlMs)));
    } else {
      reject(this.#raw.cmd("SET", key, value));
    }
  }

  get(key) {
    return reject(this.#raw.cmd("GET", key)); // Uint8Array | null
  }

  getText(key) {
    const v = this.get(key);
    return v === null ? undefined : text(v);
  }

  del(...keys) {
    return reject(this.#raw.cmd("DEL", ...keys));
  }

  incrby(key, delta) {
    return reject(this.#raw.cmd("INCRBY", key, String(delta)));
  }

  expire(key, ttlMs) {
    return reject(this.#raw.cmd("PEXPIRE", key, String(ttlMs))) === 1;
  }

  pttl(key) {
    return reject(this.#raw.cmd("PTTL", key));
  }

  keys(pattern = "*", limit) {
    const all = reject(this.#raw.cmd("KEYS", pattern)).map(text);
    return limit == null ? all : all.slice(0, limit);
  }

  mget(...keys) {
    return reject(this.#raw.cmd("MGET", ...keys));
  }

  dbsize() {
    return reject(this.#raw.cmd("DBSIZE"));
  }

  flushall() {
    reject(this.#raw.cmd("FLUSHALL"));
  }

  publish(channel, payload) {
    return reject(this.#raw.cmd("PUBLISH", channel, payload));
  }

  // Callback pub/sub, wasm-package style: the engine's queue is drained on
  // a timer (tickMs at open). The low-level polled handle stays available
  // as db.subscribeRaw() for callers who want to pump themselves.
  subscribe(channel, cb) {
    const sub = this.#raw.subscribe(channel);
    this.#subs.push({ sub, cb });
    if (!this.#timer) {
      this.#timer = setInterval(() => this.#pump(), this.#tickMs);
      // A pump keeps nothing alive on its own: an app whose only work is
      // pending messages should still be allowed to exit.
      this.#timer.unref?.();
    }
    this.#pump();
  }

  subscribeRaw(channel) {
    return this.#raw.subscribe(channel);
  }

  psubscribeRaw(pattern) {
    return this.#raw.psubscribe(pattern);
  }

  #pump() {
    for (const { sub, cb } of this.#subs) {
      for (let f = sub.next(); f !== undefined; f = sub.next()) {
        if (Array.isArray(f) && text(f[0]) === "message") {
          cb(f[2], text(f[1])); // (payload, channel)
        }
      }
    }
  }

  close() {
    if (this.#timer) clearInterval(this.#timer);
    for (const { sub } of this.#subs) sub.close();
    this.#subs = [];
    this.#raw.close();
  }
}

// Open the engine: { dir } for persistence, nothing for in-memory.
// tickMs paces the subscribe() callback pump (default 50).
export async function open(opts = {}) {
  const b = await loadBackend();
  return new Db(b.open(opts), opts.tickMs ?? 50);
}

export async function version() {
  return (await loadBackend()).version();
}

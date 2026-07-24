// @goliapkg/kevy-node — kevy embedded, one API on two runtimes.
//
// Bun loads the engine over bun:ffi; Node loads the hand-written N-API
// addon (kevy-napi). Either way this file is the API: the typed methods below
// mirror @goliapkg/kevy (the wasm package), and cmd() reaches all 184 verbs.
//
//   import { open } from "@goliapkg/kevy-node";
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

// Constant verbs, pre-encoded once. Passing bytes (not strings) to cmd() spares
// both backends a TextEncoder.encode() per call (bun.js / node.js pass Uint8Array
// through). Mirrors the ts client's VERB_CACHE.
const enc = new TextEncoder();
const V = Object.fromEntries(
  ["SET", "GET", "PX", "DEL", "INCRBY", "PEXPIRE", "PTTL", "KEYS", "MGET", "DBSIZE", "FLUSHALL", "PUBLISH"].map(
    (w) => [w, enc.encode(w)],
  ),
);

let backend;
async function loadBackend() {
  if (!backend) {
    backend = await import(isBun ? "./bun.js" : "./node.js");
  }
  return backend;
}

// A protocol error from cmd() comes back as a KevyError VALUE; the typed surface
// treats it as failure. Throw the KevyError itself — not a re-wrapped Error — so
// the taxonomy survives: `instanceof KevyError` (and each future variant) still
// holds at the catch site (contract §2.1/§2.2).
function reject(v) {
  if (v instanceof KevyError) throw v;
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
    // Scalar lane when the backend exposes it (both Bun and Node — kevy-napi
    // ships get/set as of #27).
    if (this.#raw.setScalar) {
      this.#raw.setScalar(key, value, opts.ttlMs != null ? Number(opts.ttlMs) : 0);
      return;
    }
    if (opts.ttlMs != null) {
      reject(this.#raw.cmd(V.SET, key, value, V.PX, String(opts.ttlMs)));
    } else {
      reject(this.#raw.cmd(V.SET, key, value));
    }
  }

  // Batch SET: apply many [key, value] pairs in one native crossing (the
  // batch-write path). Amortizes the per-call addon boundary a loop of set()
  // pays once per key. Falls back to a set() loop if the addon lacks setMany.
  setMany(pairs) {
    if (this.#raw.setMany) {
      this.#raw.setMany(pairs);
      return;
    }
    for (const [k, v] of pairs) this.set(k, v);
  }

  get(key) {
    // Scalar lane when available (Bun and Node — kevy-napi ships get/set as of
    // #27): skips the structural RESP-framing floor (~575 ns even at 16B; see
    // bench/PERF-FINDING-2026-07-16-embedded-get-scalar-vs-resp.md — that doc's
    // 2.0-2.6× is a Go-host microbench, the JS-runtime FFI overhead profile
    // differs, so re-measure on a JS bench before claiming a speedup).
    if (this.#raw.getScalar) {
      try {
        return this.#raw.getScalar(key); // Uint8Array | null
      } catch {
        // The shared lane collapses WRONGTYPE (GET on a non-string key) to a
        // misuse code; re-run the framed GET so the typed -WRONGTYPE surfaces.
        return reject(this.#raw.cmd(V.GET, key));
      }
    }
    return reject(this.#raw.cmd(V.GET, key)); // Uint8Array | null
  }

  getText(key) {
    const v = this.get(key);
    return v === null ? undefined : text(v);
  }

  del(...keys) {
    return reject(this.#raw.cmd(V.DEL, ...keys));
  }

  incrby(key, delta) {
    return reject(this.#raw.cmd(V.INCRBY, key, String(delta)));
  }

  expire(key, ttlMs) {
    return reject(this.#raw.cmd(V.PEXPIRE, key, String(ttlMs))) === 1;
  }

  pttl(key) {
    return reject(this.#raw.cmd(V.PTTL, key));
  }

  keys(pattern = "*", limit) {
    const all = reject(this.#raw.cmd(V.KEYS, pattern)).map(text);
    return limit == null ? all : all.slice(0, limit);
  }

  mget(...keys) {
    return reject(this.#raw.cmd(V.MGET, ...keys));
  }

  dbsize() {
    return reject(this.#raw.cmd(V.DBSIZE));
  }

  flushall() {
    reject(this.#raw.cmd(V.FLUSHALL));
  }

  publish(channel, payload) {
    return reject(this.#raw.cmd(V.PUBLISH, channel, payload));
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

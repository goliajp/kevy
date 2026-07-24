// The Node door: the hand-written N-API addon (kevy-napi) over the same
// engine. Mirrors bun.js's backend contract one-to-one; resp.js turns
// replies into JS values.
//
//   import { open } from "@goliapkg/kevy-node/node.js";
//   const db = open({ dir: "data/" });        // or open() for in-memory
//   db.cmd("SET", "k", "v");
//   text(db.cmd("GET", "k"))                  // "v"
//
// argv crosses the addon as one flat Buffer — a u32-LE length prefix per
// argument — so the native side needs no array or string APIs.

import { createRequire } from "node:module";

import { KevyError, parse, text } from "./resp.js";

export { KevyError, text };

const LIB = process.env.KEVY_NAPI_LIB ?? defaultLibPath();

function defaultLibPath() {
  // Published layout first: the platform package npm picked from
  // optionalDependencies (see packaging/npm/gen-node-platform-pkg.sh).
  try {
    return createRequire(import.meta.url).resolve(
      `@goliapkg/kevy-node-${process.platform}-${process.arch}/kevy.node`,
    );
  } catch {
    // In-repo layout (development / ffigate).
    const ext = process.platform === "darwin" ? "dylib" : "so";
    return new URL(`../../target/debug/libkevy_napi.${ext}`, import.meta.url).pathname;
  }
}

// process.dlopen is the one loader that takes an arbitrary path and
// extension; it fills mod.exports from napi_register_module_v1.
const mod = { exports: {} };
process.dlopen(mod, LIB);
const c = mod.exports;

const enc = new TextEncoder();

function toBytes(a) {
  return a instanceof Uint8Array ? a : enc.encode(String(a));
}

// One flat Buffer: u32-LE length prefix, then the bytes, per argument.
function pack(argv) {
  const bufs = argv.map(toBytes);
  let total = 0;
  for (const b of bufs) total += 4 + b.length;
  const out = Buffer.allocUnsafe(total);
  let pos = 0;
  for (const b of bufs) {
    out.writeUInt32LE(b.length, pos);
    out.set(b, pos + 4);
    pos += 4 + b.length;
  }
  return out;
}

class Sub {
  #p;
  constructor(p) {
    this.#p = p;
  }
  next() {
    const f = c.subNext(this.#p);
    return f === undefined ? undefined : parse(f);
  }
  // Blocking wait for one frame (timeoutMs 0 = forever); undefined on timeout.
  // WARNING: parks the calling OS thread for the whole wait — only safe on a
  // dedicated worker_thread, NEVER the main event loop (it stalls all of Node).
  wait(timeoutMs = 0) {
    const f = c.subWait(this.#p, timeoutMs);
    return f === undefined ? undefined : parse(f);
  }
  close() {
    if (this.#p) c.subClose(this.#p);
    this.#p = null;
  }
}

class Db {
  #p;
  constructor(p) {
    this.#p = p;
  }

  // Run one command; argv[0] is the verb. Strings are UTF-8 encoded;
  // Uint8Arrays pass through binary-safe. A protocol error comes back as a
  // KevyError VALUE — check `instanceof`, it is data.
  cmd(...argv) {
    if (!this.#p) throw new Error("kevy: closed handle");
    const reply = c.cmd(this.#p, pack(argv));
    return reply === null ? null : parse(reply);
  }

  // Scalar fast GET: skips argv-pack + RESP framing, riding the engine's
  // zero-copy shared lane. Returns Uint8Array (a Buffer) | null — the raw value,
  // no RESP. A GET on a non-string key collapses to a misuse code (WRONGTYPE is
  // unrepresentable on this lane) and throws; index.js catches it and re-runs a
  // framed GET so the typed -WRONGTYPE error surfaces. A present-but-empty value
  // is a Buffer of length 0, NOT null.
  getScalar(key) {
    if (!this.#p) throw new Error("kevy: closed handle");
    return c.get(this.#p, Buffer.from(toBytes(key))); // Buffer | null
  }

  // Scalar fast SET (ttlMs 0 = no TTL). No WRONGTYPE hazard: SET overwrites.
  setScalar(key, value, ttlMs = 0) {
    if (!this.#p) throw new Error("kevy: closed handle");
    c.set(this.#p, Buffer.from(toBytes(key)), Buffer.from(toBytes(value)), ttlMs);
  }

  // Batch SET: apply many [key, value] pairs in one addon crossing (the
  // batch-write path). `pairs` is any iterable of [key, value]. Packed into
  // ~1 MiB chunks so memory stays bounded regardless of batch size; each set
  // appends to the AOF (durability unchanged).
  setMany(pairs) {
    if (!this.#p) throw new Error("kevy: closed handle");
    const CHUNK = 1 << 20;
    let flat = [];
    let bytes = 0;
    const flush = () => {
      if (flat.length) c.setMany(this.#p, pack(flat));
      flat = [];
      bytes = 0;
    };
    for (const [k, v] of pairs) {
      const kb = toBytes(k);
      const vb = toBytes(v);
      const need = 8 + kb.length + vb.length;
      if (bytes > 0 && bytes + need > CHUNK) flush();
      flat.push(kb, vb);
      bytes += need;
    }
    flush();
  }

  // The boot-replay verdict: what this open restored — and what it could not,
  // as { replayedCommands, replayedBytes, elapsedMs, droppedBytes, corrupt,
  // quarantineCount } (numbers; corrupt is a boolean). droppedBytes > 0 or
  // corrupt means the store recovered LESS than its files held (the dropped
  // region was quarantined next to the AOF): surface it as a startup health
  // check instead of scraping the boot WARN line from stderr.
  openReport() {
    if (!this.#p) throw new Error("kevy: closed handle");
    return c.openReport(this.#p);
  }

  // Deterministic teardown: flush every shard's AOF (a REAL fsync), then
  // refuse every later write. Reads stay available, so the handle stays
  // live — close() when done. Idempotent; the signal-handler exit is
  // `db.shutdown(); process.exit(0)`. Throws on an I/O failure (the store
  // is still usable; retry or exit).
  shutdown() {
    if (!this.#p) throw new Error("kevy: closed handle");
    c.shutdown(this.#p);
  }

  subscribe(channel) {
    return new Sub(c.subscribe(this.#p, Buffer.from(toBytes(channel))));
  }

  psubscribe(pattern) {
    return new Sub(c.psubscribe(this.#p, Buffer.from(toBytes(pattern))));
  }

  close() {
    if (this.#p) c.close(this.#p);
    this.#p = null;
  }
}

export function open(opts = {}) {
  const p = opts.dir ? c.open(Buffer.from(enc.encode(opts.dir))) : c.openMem();
  return new Db(p);
}

// open() with explicit durability/rewrite policy. dir null = in-memory.
// opts: { fsync (0 everysec, 1 always, 2 no), shards, rewritePct,
// rewriteMinSize, rewriteBytes, rewriteIntervalSecs } — every field
// optional, missing = the exact defaults open() uses; opts null = all
// defaults.
export function openWith(dir = null, opts = null) {
  const d = dir ? Buffer.from(enc.encode(dir)) : null;
  return new Db(c.openWith(d, opts));
}

export function version() {
  return c.version();
}

export function abi() {
  return c.abi();
}

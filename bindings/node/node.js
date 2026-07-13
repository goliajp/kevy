// The Node door: the hand-written N-API addon (kevy-napi) over the same
// engine. Mirrors bun.js's backend contract one-to-one; resp.js turns
// replies into JS values.
//
//   import { open } from "@goliajp/kevy-node/node.js";
//   const db = open({ dir: "data/" });        // or open() for in-memory
//   db.cmd("SET", "k", "v");
//   text(db.cmd("GET", "k"))                  // "v"
//
// argv crosses the addon as one flat Buffer — a u32-LE length prefix per
// argument — so the native side needs no array or string APIs.

import { KevyError, parse, text } from "./resp.js";

export { KevyError, text };

const LIB = process.env.KEVY_NAPI_LIB ?? defaultLibPath();

function defaultLibPath() {
  const ext = process.platform === "darwin" ? "dylib" : "so";
  // In-repo layout first (development / ffigate), then alongside this file
  // (the published package ships the addon next to the loader).
  return new URL(`../../target/debug/libkevy_napi.${ext}`, import.meta.url).pathname;
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

export function version() {
  return c.version();
}

export function abi() {
  return c.abi();
}

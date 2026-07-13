// The Bun door: bun:ffi straight onto the kevy cdylib — no addon, no
// build step. Mirrors the C ABI one-to-one; resp.js turns replies into
// JS values.
//
//   import { open } from "@goliajp/kevy-node/bun.js";
//   const db = open({ dir: "data/" });        // or open() for in-memory
//   db.cmd("SET", "k", "v");
//   text(db.cmd("GET", "k"))                  // "v"

import { dlopen, FFIType, ptr, toArrayBuffer } from "bun:ffi";

import { KevyError, parse, text } from "./resp.js";

export { KevyError, text };

const LIB = process.env.KEVY_FFI_LIB ?? defaultLibPath();

function defaultLibPath() {
  const ext = process.platform === "darwin" ? "dylib" : "so";
  // In-repo layout first (development / ffigate), then alongside this file
  // (the published package ships the library next to the loader).
  return new URL(`../../target/debug/libkevy_ffi.${ext}`, import.meta.url).pathname;
}

const c = dlopen(LIB, {
  kevy_open: { args: [FFIType.ptr, FFIType.u64], returns: FFIType.ptr },
  kevy_open_mem: { args: [], returns: FFIType.ptr },
  kevy_close: { args: [FFIType.ptr], returns: FFIType.void },
  kevy_cmd: {
    args: [FFIType.ptr, FFIType.u64, FFIType.ptr, FFIType.ptr, FFIType.ptr],
    returns: FFIType.i32,
  },
  kevy_buf_free: { args: [FFIType.ptr, FFIType.u64, FFIType.u64], returns: FFIType.void },
  kevy_subscribe: { args: [FFIType.ptr, FFIType.ptr, FFIType.u64], returns: FFIType.ptr },
  kevy_psubscribe: { args: [FFIType.ptr, FFIType.ptr, FFIType.u64], returns: FFIType.ptr },
  kevy_sub_next: { args: [FFIType.ptr, FFIType.ptr], returns: FFIType.i32 },
  kevy_sub_close: { args: [FFIType.ptr], returns: FFIType.void },
  kevy_version: { args: [], returns: FFIType.cstring },
  kevy_abi: { args: [], returns: FFIType.u32 },
}).symbols;

// KevyBuf is {ptr, len, cap} of pointer-sized fields; one scratch struct is
// enough because cmd() consumes it before returning. 64-bit only — every
// platform kevy ships on.
const OUT = new BigUint64Array(3);
const enc = new TextEncoder();

function takeReply() {
  const [p, len, cap] = OUT;
  if (len === 0n) return null;
  // Copy out, then free the Rust buffer — the ArrayBuffer view borrows.
  const view = new Uint8Array(toArrayBuffer(Number(p), 0, Number(len)));
  const bytes = view.slice();
  c.kevy_buf_free(Number(p), Number(len), Number(cap));
  return parse(bytes);
}

class Sub {
  #p;
  constructor(p) {
    this.#p = p;
  }
  next() {
    const rc = c.kevy_sub_next(this.#p, ptr(OUT));
    if (rc < 0) throw new Error("kevy: subscription misuse");
    if (rc === 0) return undefined;
    return takeReply();
  }
  close() {
    if (this.#p) c.kevy_sub_close(this.#p);
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
    const bufs = argv.map((a) => (a instanceof Uint8Array ? a : enc.encode(String(a))));
    const ptrs = new BigUint64Array(bufs.length);
    const lens = new BigUint64Array(bufs.length);
    for (let i = 0; i < bufs.length; i++) {
      // Zero-length: any non-null stable pointer works; reuse OUT.
      ptrs[i] = BigInt(bufs[i].length ? ptr(bufs[i]) : ptr(OUT));
      lens[i] = BigInt(bufs[i].length);
    }
    const rc = c.kevy_cmd(this.#p, bufs.length, ptr(ptrs), ptr(lens), ptr(OUT));
    if (rc !== 0) throw new Error("kevy: kevy_cmd misuse");
    return takeReply();
  }

  subscribe(channel) {
    const b = enc.encode(channel);
    const p = c.kevy_subscribe(this.#p, ptr(b), b.length);
    if (!p) throw new Error("kevy: subscribe failed");
    return new Sub(p);
  }

  psubscribe(pattern) {
    const b = enc.encode(pattern);
    const p = c.kevy_psubscribe(this.#p, ptr(b), b.length);
    if (!p) throw new Error("kevy: psubscribe failed");
    return new Sub(p);
  }

  close() {
    if (this.#p) c.kevy_close(this.#p);
    this.#p = null;
  }
}

export function open(opts = {}) {
  let p;
  if (opts.dir) {
    const b = enc.encode(opts.dir);
    p = c.kevy_open(ptr(b), b.length);
  } else {
    p = c.kevy_open_mem();
  }
  if (!p) throw new Error("kevy: open failed");
  return new Db(p);
}

export function version() {
  return c.kevy_version().toString();
}

export function abi() {
  return c.kevy_abi();
}

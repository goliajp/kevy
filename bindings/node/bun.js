// The Bun door: bun:ffi straight onto the kevy cdylib — no addon, no
// build step. Mirrors the C ABI one-to-one; resp.js turns replies into
// JS values.
//
//   import { open } from "@goliapkg/kevy-node/bun.js";
//   const db = open({ dir: "data/" });        // or open() for in-memory
//   db.cmd("SET", "k", "v");
//   text(db.cmd("GET", "k"))                  // "v"

import { dlopen, FFIType, ptr, toArrayBuffer } from "bun:ffi";

import { KevyError, parse, text } from "./resp.js";

export { KevyError, text };

const LIB = process.env.KEVY_FFI_LIB ?? defaultLibPath();

function defaultLibPath() {
  const ext = process.platform === "darwin" ? "dylib" : "so";
  // Published layout first: the platform package npm/bun picked from
  // optionalDependencies (see packaging/npm/gen-node-platform-pkg.sh).
  try {
    return import.meta.require.resolve(
      `@goliapkg/kevy-node-${process.platform}-${process.arch}/libkevy_ffi.${ext}`,
    );
  } catch {
    // In-repo layout (development / ffigate).
    return new URL(`../../target/debug/libkevy_ffi.${ext}`, import.meta.url).pathname;
  }
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
  // Scalar fast lanes (Bun only — the cdylib exports these; the N-API addon
  // does not until #27). GET rides the zero-copy shared lane; free its reply
  // with kevy_buf_free_shared, NOT kevy_buf_free.
  kevy_get_shared: { args: [FFIType.ptr, FFIType.ptr, FFIType.u64, FFIType.ptr], returns: FFIType.i32 },
  kevy_buf_free_shared: { args: [FFIType.ptr, FFIType.u64, FFIType.u64], returns: FFIType.void },
  kevy_set: {
    args: [FFIType.ptr, FFIType.ptr, FFIType.u64, FFIType.ptr, FFIType.u64, FFIType.u64],
    returns: FFIType.i32,
  },
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

// Consume the shared-lane OUT (kevy_get_shared): the bytes are a VIEW of the
// engine's Arc'd value (no engine-side copy for a bulk); copy once, then release
// the owner. Free with kevy_buf_free_shared — `cap` is an OPAQUE owner handle
// (an Arc raw pointer or a tagged Vec), NOT a capacity; kevy_buf_free would
// misread it. A present-but-empty value returns an empty array, not null.
function takeShared() {
  const [p, len, cap] = OUT;
  const n = Number(len);
  const bytes = n === 0 ? new Uint8Array(0) : new Uint8Array(toArrayBuffer(Number(p), 0, n)).slice();
  c.kevy_buf_free_shared(Number(p), n, Number(cap));
  return bytes;
}

// A key/value arg as raw bytes: Uint8Array through, strings UTF-8 encoded.
function argBytes(a) {
  return a instanceof Uint8Array ? a : enc.encode(String(a));
}

// A stable non-null pointer for a possibly-empty buffer (len 0 → engine reads
// nothing, but the pointer must be non-null; reuse OUT's address).
function nzp(b) {
  return b.length ? ptr(b) : ptr(OUT);
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
    const bufs = argv.map(argBytes);
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

  // Scalar fast GET: skips argv-pack + RESP framing, riding the engine's
  // zero-copy shared lane. Returns Uint8Array | null (the raw value, no RESP).
  // A GET on a non-string key collapses to rc -2 (WRONGTYPE is unrepresentable
  // on this lane) and throws here; index.js catches it and re-runs a framed
  // GET so the typed -WRONGTYPE error surfaces.
  getScalar(key) {
    if (!this.#p) throw new Error("kevy: closed handle");
    const kb = argBytes(key);
    const rc = c.kevy_get_shared(this.#p, nzp(kb), kb.length, ptr(OUT));
    if (rc < 0) throw new Error("kevy: kevy_get_shared misuse");
    return rc === 0 ? null : takeShared();
  }

  // Scalar fast SET (ttlMs 0 = no TTL). No WRONGTYPE hazard: SET overwrites.
  setScalar(key, value, ttlMs = 0) {
    if (!this.#p) throw new Error("kevy: closed handle");
    const kb = argBytes(key);
    const vb = argBytes(value);
    const rc = c.kevy_set(this.#p, nzp(kb), kb.length, nzp(vb), vb.length, ttlMs);
    if (rc < 0) throw new Error("kevy: kevy_set misuse or storage error");
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

// The embedded engine door (contract §5.1). Loads the kevy C ABI two ways —
// bun:ffi straight onto the cdylib on Bun, the hand-written N-API addon
// (kevy-napi) on Node — behind one uniform low-level handle that hands back
// raw RESP bytes. The higher-level EmbeddedDb (§5.2) parses those into Reply.
//
// The Bun path uses the real scalar (kevy_get/kevy_set) and blocking-wait
// (kevy_sub_wait) symbols; the N-API addon exposes only the ten generic
// symbols, so Node emulates the scalar path via cmd and the blocking wait via
// a bounded poll (observably identical, contract §3.14 note).

import { createRequire } from "node:module";

const isBun = typeof (globalThis as { Bun?: unknown }).Bun !== "undefined";

/** A low-level subscription handle over one channel/pattern. */
export interface RawSub {
  /** Poll one frame's raw RESP bytes, or null when nothing is queued. */
  nextRaw(): Uint8Array | null;
  /** Block up to timeoutMs (0 = forever) for one frame; null on timeout. */
  waitRaw(timeoutMs: number): Uint8Array | null;
  close(): void;
}

/** A low-level open store handle. */
export interface RawHandle {
  /** Run one command; returns the raw RESP reply bytes (null if empty). */
  cmdRaw(argv: Uint8Array[]): Uint8Array | null;
  /** Scalar fast GET, or null on miss (§5.2). */
  getScalar(key: Uint8Array): Uint8Array | null;
  /** Scalar fast SET (ttlMs 0 = no TTL). */
  setScalar(key: Uint8Array, val: Uint8Array, ttlMs: number): void;
  subscribe(name: Uint8Array, pattern: boolean): RawSub;
  close(): void;
}

/** The loaded engine — the factory for store handles. */
export interface RawEngine {
  open(dir: string): RawHandle;
  openMem(): RawHandle;
  version(): string;
  abi(): number;
}

let cached: RawEngine | undefined;

/** Load (once) the embedded engine for the current runtime. */
export function loadEngine(): RawEngine {
  if (!cached) cached = isBun ? loadBun() : loadNode();
  return cached;
}

// A bounded synchronous sleep (no busy-spin), used by the Node poll paths.
const SLEEP_BUF = new Int32Array(new SharedArrayBuffer(4));
export function sleepSync(ms: number): void {
  Atomics.wait(SLEEP_BUF, 0, 0, Math.max(1, ms));
}

// --- Bun: bun:ffi straight onto the cdylib ------------------------------

function loadBun(): RawEngine {
  // Resolved lazily so Node never evaluates the bun:ffi specifier; Bun's
  // createRequire resolves the built-in synchronously.
  const ffi = createRequire(import.meta.url)("bun:ffi") as typeof import("bun:ffi");
  const { dlopen, FFIType, ptr, toArrayBuffer } = ffi;
  const lib = process.env.KEVY_FFI_LIB ?? defaultLib("libkevy_ffi");
  const c = dlopen(lib, {
    kevy_open: { args: [FFIType.ptr, FFIType.u64], returns: FFIType.ptr },
    kevy_open_mem: { args: [], returns: FFIType.ptr },
    kevy_close: { args: [FFIType.ptr], returns: FFIType.void },
    kevy_cmd: {
      args: [FFIType.ptr, FFIType.u64, FFIType.ptr, FFIType.ptr, FFIType.ptr],
      returns: FFIType.i32,
    },
    kevy_buf_free: { args: [FFIType.ptr, FFIType.u64, FFIType.u64], returns: FFIType.void },
    kevy_get: { args: [FFIType.ptr, FFIType.ptr, FFIType.u64, FFIType.ptr], returns: FFIType.i32 },
    kevy_set: {
      args: [FFIType.ptr, FFIType.ptr, FFIType.u64, FFIType.ptr, FFIType.u64, FFIType.u64],
      returns: FFIType.i32,
    },
    kevy_subscribe: { args: [FFIType.ptr, FFIType.ptr, FFIType.u64], returns: FFIType.ptr },
    kevy_psubscribe: { args: [FFIType.ptr, FFIType.ptr, FFIType.u64], returns: FFIType.ptr },
    kevy_sub_next: { args: [FFIType.ptr, FFIType.ptr], returns: FFIType.i32 },
    kevy_sub_wait: { args: [FFIType.ptr, FFIType.u64, FFIType.ptr], returns: FFIType.i32 },
    kevy_sub_close: { args: [FFIType.ptr], returns: FFIType.void },
    kevy_version: { args: [], returns: FFIType.cstring },
    kevy_abi: { args: [], returns: FFIType.u32 },
  }).symbols;

  const OUT = new BigUint64Array(3);
  // FFIType.ptr args want a plain number pointer; zero-length buffers reuse
  // OUT's stable non-null address (any works, len 0 means kevy reads nothing).
  const nzp = (b: Uint8Array): number => (b.length ? (ptr(b) as number) : (ptr(OUT) as number));
  const take = (): Uint8Array | null => {
    const len = Number(OUT[1]);
    if (len === 0) return null;
    const p = Number(OUT[0]);
    const copy = new Uint8Array(toArrayBuffer(p, 0, len)).slice();
    c.kevy_buf_free(p, len, Number(OUT[2]));
    return copy;
  };

  const makeSub = (p: number): RawSub => ({
    nextRaw() {
      const rc = c.kevy_sub_next(p, ptr(OUT));
      if (rc < 0) throw new Error("kevy: subscription misuse");
      return rc === 0 ? null : take();
    },
    waitRaw(timeoutMs) {
      const rc = c.kevy_sub_wait(p, timeoutMs, ptr(OUT));
      if (rc < 0) throw new Error("kevy: subscription closed");
      return rc === 0 ? null : take();
    },
    close() {
      if (p) c.kevy_sub_close(p);
    },
  });

  const makeHandle = (dbp: number): RawHandle => ({
    cmdRaw(argv) {
      const bufs = argv;
      const ptrs = new BigUint64Array(bufs.length);
      const lens = new BigUint64Array(bufs.length);
      for (let i = 0; i < bufs.length; i++) {
        ptrs[i] = BigInt(nzp(bufs[i]!));
        lens[i] = BigInt(bufs[i]!.length);
      }
      const rc = c.kevy_cmd(dbp, bufs.length, ptr(ptrs), ptr(lens), ptr(OUT));
      if (rc !== 0) throw new Error("kevy: kevy_cmd misuse");
      return take();
    },
    getScalar(key) {
      const rc = c.kevy_get(dbp, nzp(key), key.length, ptr(OUT));
      if (rc < 0) throw new Error("kevy: kevy_get misuse");
      return rc === 0 ? null : take();
    },
    setScalar(key, val, ttlMs) {
      const rc = c.kevy_set(dbp, nzp(key), key.length, nzp(val), val.length, ttlMs);
      if (rc < 0) throw new Error("kevy: kevy_set misuse or storage error");
    },
    subscribe(name, pattern) {
      const p = pattern
        ? c.kevy_psubscribe(dbp, nzp(name), name.length)
        : c.kevy_subscribe(dbp, nzp(name), name.length);
      if (!p) throw new Error("kevy: subscribe failed");
      return makeSub(Number(p));
    },
    close() {
      if (dbp) c.kevy_close(dbp);
    },
  });

  const enc = new TextEncoder();
  return {
    open(dir) {
      const b = enc.encode(dir);
      const p = c.kevy_open(ptr(b), b.length);
      if (!p) throw new Error("kevy: open failed");
      return makeHandle(Number(p));
    },
    openMem() {
      const p = c.kevy_open_mem();
      if (!p) throw new Error("kevy: open failed");
      return makeHandle(Number(p));
    },
    version: () => c.kevy_version().toString(),
    abi: () => c.kevy_abi(),
  };
}

// --- Node: the N-API addon (kevy-napi) ----------------------------------

interface NapiExports {
  open(dir: Buffer): unknown;
  openMem(): unknown;
  close(p: unknown): void;
  cmd(p: unknown, packed: Buffer): Uint8Array | null;
  subscribe(p: unknown, name: Buffer): unknown;
  psubscribe(p: unknown, pat: Buffer): unknown;
  subNext(p: unknown): Uint8Array | null | undefined;
  subClose(p: unknown): void;
  version(): string;
  abi(): number;
}

function loadNode(): RawEngine {
  const lib = process.env.KEVY_NAPI_LIB ?? defaultLib("libkevy_napi");
  const mod = { exports: {} as NapiExports };
  (process as unknown as { dlopen(m: object, p: string): void }).dlopen(mod, lib);
  const c = mod.exports;
  const enc = new TextEncoder();

  // argv crosses the addon as one flat Buffer: a u32-LE length prefix per arg.
  const pack = (argv: Uint8Array[]): Buffer => {
    let total = 0;
    for (const a of argv) total += 4 + a.length;
    const out = Buffer.allocUnsafe(total);
    let pos = 0;
    for (const a of argv) {
      out.writeUInt32LE(a.length, pos);
      out.set(a, pos + 4);
      pos += 4 + a.length;
    }
    return out;
  };

  const makeSub = (p: unknown): RawSub => ({
    nextRaw() {
      const f = c.subNext(p);
      return f == null ? null : f;
    },
    waitRaw(timeoutMs) {
      // The N-API addon has no blocking wait; poll until timeout (0 = forever).
      const forever = timeoutMs === 0;
      const end = Date.now() + timeoutMs;
      for (;;) {
        const f = c.subNext(p);
        if (f != null) return f;
        if (!forever && Date.now() >= end) return null;
        sleepSync(2);
      }
    },
    close() {
      c.subClose(p);
    },
  });

  const makeHandle = (dbp: unknown): RawHandle => ({
    cmdRaw(argv) {
      return c.cmd(dbp, pack(argv));
    },
    getScalar(key) {
      const r = c.cmd(dbp, pack([enc.encode("GET"), key]));
      if (r == null) return null;
      // Reply is a $bulk or $-1; decode the bulk body lazily in EmbeddedDb.
      return decodeBulk(r);
    },
    setScalar(key, val, ttlMs) {
      const argv =
        ttlMs > 0
          ? [enc.encode("SET"), key, val, enc.encode("PX"), enc.encode(String(ttlMs))]
          : [enc.encode("SET"), key, val];
      c.cmd(dbp, pack(argv));
    },
    subscribe(name, pattern) {
      const p = pattern
        ? c.psubscribe(dbp, Buffer.from(name))
        : c.subscribe(dbp, Buffer.from(name));
      return makeSub(p);
    },
    close() {
      c.close(dbp);
    },
  });

  return {
    open(dir) {
      return makeHandle(c.open(Buffer.from(enc.encode(dir))));
    },
    openMem() {
      return makeHandle(c.openMem());
    },
    version: () => c.version(),
    abi: () => c.abi(),
  };
}

// Extract a $bulk body (or null for $-1) from a raw RESP GET reply.
function decodeBulk(raw: Uint8Array): Uint8Array | null {
  if (raw[0] !== 0x24) return null; // not a bulk
  let i = 1;
  while (i + 1 < raw.length && !(raw[i] === 0x0d && raw[i + 1] === 0x0a)) i++;
  const n = Number(new TextDecoder().decode(raw.subarray(1, i)));
  if (n < 0) return null;
  const start = i + 2;
  return raw.slice(start, start + n);
}

function defaultLib(stem: string): string {
  const ext = process.platform === "darwin" ? "dylib" : "so";
  const require = createRequire(import.meta.url);
  try {
    const which = stem === "libkevy_napi" ? "kevy.node" : `${stem}.${ext}`;
    return require.resolve(`@goliapkg/kevy-node-${process.platform}-${process.arch}/${which}`);
  } catch {
    return new URL(`../../../target/debug/${stem}.${ext}`, import.meta.url).pathname;
  }
}

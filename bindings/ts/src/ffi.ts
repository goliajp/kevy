// The embedded engine door (contract §5.1). Loads the kevy C ABI two ways —
// bun:ffi straight onto the cdylib on Bun, the hand-written N-API addon
// (kevy-napi) on Node — behind one uniform low-level handle that hands back
// raw RESP bytes. The higher-level EmbeddedDb (§5.2) parses those into Reply.
//
// The Bun path uses the real scalar (kevy_get_shared/kevy_set) and blocking-wait
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
/** Bounded async sleep (the poll cadence for blocking pops / the embedded bus). */
export const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

// --- Bun: bun:ffi straight onto the cdylib ------------------------------

// A pointer as bun:ffi hands it back: a plain JS number (48-bit VA fits in a
// double). u64 args accept a number too, so every parameter below is `number`.
type Ptr = number;

// The kevy C ABI surface this client binds, typed so the module-level helpers
// can carry `c` without leaking bun:ffi's `any` symbols into business code.
interface BunSymbols {
  kevy_open(dir: Ptr, dirLen: number): Ptr;
  kevy_open_mem(): Ptr;
  kevy_close(db: Ptr): void;
  kevy_cmd(db: Ptr, argc: number, argv: Ptr, lens: Ptr, out: Ptr): number;
  kevy_buf_free(ptr: Ptr, len: number, cap: number): void;
  kevy_get_shared(db: Ptr, key: Ptr, keyLen: number, out: Ptr): number;
  kevy_buf_free_shared(ptr: Ptr, len: number, cap: number): void;
  kevy_set(db: Ptr, key: Ptr, keyLen: number, val: Ptr, valLen: number, ttlMs: number): number;
  kevy_subscribe(db: Ptr, name: Ptr, nameLen: number): Ptr;
  kevy_psubscribe(db: Ptr, name: Ptr, nameLen: number): Ptr;
  kevy_sub_next(sub: Ptr, out: Ptr): number;
  kevy_sub_wait(sub: Ptr, timeoutMs: number, out: Ptr): number;
  kevy_sub_close(sub: Ptr): void;
  kevy_version(): { toString(): string };
  kevy_abi(): number;
}

// The bits every Bun helper needs: the symbols, the two `ptr`/`toArrayBuffer`
// address helpers, the shared 3-word OUT struct, and the non-null-ptr coercer.
interface BunCtx {
  c: BunSymbols;
  ptr: (view: ArrayBufferView) => number;
  toArrayBuffer: (ptr: number, off: number, len: number) => ArrayBuffer;
  out: BigUint64Array;
  nzp: (b: Uint8Array) => number;
}

// Scratch argv pointer/length vectors for kevy_cmd, reused across calls (the
// engine is driven single-threaded and kevy_cmd is synchronous, so one pair is
// enough); grown on demand when a command carries more args than any before.
let ptrScratch = new BigUint64Array(8);
let lenScratch = new BigUint64Array(8);
function scratchFor(n: number): [BigUint64Array, BigUint64Array] {
  if (ptrScratch.length < n) {
    ptrScratch = new BigUint64Array(n);
    lenScratch = new BigUint64Array(n);
  }
  return [ptrScratch, lenScratch];
}

// Consume the plain-lane OUT buffer (kevy_cmd / pub-sub): copy the reply out
// and free it. null when the reply is empty.
function bunTake(ctx: BunCtx): Uint8Array | null {
  const len = Number(ctx.out[1]);
  if (len === 0) return null;
  const p = Number(ctx.out[0]);
  const copy = new Uint8Array(ctx.toArrayBuffer(p, 0, len)).slice();
  ctx.c.kevy_buf_free(p, len, Number(ctx.out[2]));
  return copy;
}

// Consume the shared-lane OUT buffer (kevy_get_shared): the value is a VIEW of
// the engine's Arc'd bytes for a large bulk (no engine-side copy). We still copy
// once into the returned Uint8Array, then release the engine owner via the
// shared free. Only the caller's hit branch reaches here. An empty (but present)
// value returns an empty array, not null.
function bunTakeShared(ctx: BunCtx): Uint8Array {
  const p = Number(ctx.out[0]);
  const len = Number(ctx.out[1]);
  const cap = Number(ctx.out[2]);
  const copy = len === 0 ? new Uint8Array(0) : new Uint8Array(ctx.toArrayBuffer(p, 0, len)).slice();
  ctx.c.kevy_buf_free_shared(p, len, cap);
  return copy;
}

function bunMakeSub(ctx: BunCtx, p: number): RawSub {
  const { c, ptr, out } = ctx;
  return {
    nextRaw() {
      const rc = c.kevy_sub_next(p, ptr(out));
      if (rc < 0) throw new Error("kevy: subscription misuse");
      return rc === 0 ? null : bunTake(ctx);
    },
    waitRaw(timeoutMs) {
      const rc = c.kevy_sub_wait(p, timeoutMs, ptr(out));
      if (rc < 0) throw new Error("kevy: subscription closed");
      return rc === 0 ? null : bunTake(ctx);
    },
    close() {
      if (p) c.kevy_sub_close(p);
    },
  };
}

function bunMakeHandle(ctx: BunCtx, dbp: number): RawHandle {
  const { c, ptr, out, nzp } = ctx;
  return {
    cmdRaw(argv) {
      const n = argv.length;
      const [ptrs, lens] = scratchFor(n);
      for (let i = 0; i < n; i++) {
        ptrs[i] = BigInt(nzp(argv[i]!));
        lens[i] = BigInt(argv[i]!.length);
      }
      const rc = c.kevy_cmd(dbp, n, ptr(ptrs), ptr(lens), ptr(out));
      if (rc !== 0) throw new Error("kevy: kevy_cmd misuse");
      return bunTake(ctx);
    },
    getScalar(key) {
      // Zero-copy shared lane: a large bulk value is handed out as a view of the
      // engine's Arc (a refcount bump, no engine-side copy); a small value is a
      // single fresh Vec, byte-identical cost to the plain lane. The JS copy into
      // the returned array still happens either way — so the win is large-value
      // GET only (it saves the engine's Arc-clone-to-fresh-Vec copy).
      const rc = c.kevy_get_shared(dbp, nzp(key), key.length, ptr(out));
      if (rc < 0) throw new Error("kevy: kevy_get_shared misuse");
      return rc === 0 ? null : bunTakeShared(ctx);
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
      return bunMakeSub(ctx, Number(p));
    },
    close() {
      if (dbp) c.kevy_close(dbp);
    },
  };
}

function loadBun(): RawEngine {
  // Resolved lazily so Node never evaluates the bun:ffi specifier; Bun's
  // createRequire resolves the built-in synchronously.
  const ffi = createRequire(import.meta.url)("bun:ffi") as typeof import("bun:ffi");
  const { dlopen, FFIType, ptr, toArrayBuffer } = ffi;
  const lib = process.env.KEVY_FFI_LIB ?? defaultLib("libkevy_ffi");
  const c = dlopen(lib, bunTable(FFIType)).symbols as BunSymbols;

  const out = new BigUint64Array(3);
  // FFIType.ptr args want a plain number pointer; zero-length buffers reuse
  // OUT's stable non-null address (any works, len 0 means kevy reads nothing).
  const nzp = (b: Uint8Array): number => (b.length ? (ptr(b) as number) : (ptr(out) as number));
  const ctx: BunCtx = { c, ptr, toArrayBuffer, out, nzp };
  const enc = new TextEncoder();
  return {
    open(dir) {
      const b = enc.encode(dir);
      const p = c.kevy_open(ptr(b), b.length);
      if (!p) throw new Error("kevy: open failed");
      return bunMakeHandle(ctx, Number(p));
    },
    openMem() {
      const p = c.kevy_open_mem();
      if (!p) throw new Error("kevy: open failed");
      return bunMakeHandle(ctx, Number(p));
    },
    version: () => c.kevy_version().toString(),
    abi: () => c.kevy_abi(),
  };
}

// The dlopen symbol table (data, not control flow — kept as one map).
function bunTable(t: typeof import("bun:ffi").FFIType) {
  return {
    kevy_open: { args: [t.ptr, t.u64], returns: t.ptr },
    kevy_open_mem: { args: [], returns: t.ptr },
    kevy_close: { args: [t.ptr], returns: t.void },
    kevy_cmd: { args: [t.ptr, t.u64, t.ptr, t.ptr, t.ptr], returns: t.i32 },
    kevy_buf_free: { args: [t.ptr, t.u64, t.u64], returns: t.void },
    kevy_get_shared: { args: [t.ptr, t.ptr, t.u64, t.ptr], returns: t.i32 },
    kevy_buf_free_shared: { args: [t.ptr, t.u64, t.u64], returns: t.void },
    kevy_set: { args: [t.ptr, t.ptr, t.u64, t.ptr, t.u64, t.u64], returns: t.i32 },
    kevy_subscribe: { args: [t.ptr, t.ptr, t.u64], returns: t.ptr },
    kevy_psubscribe: { args: [t.ptr, t.ptr, t.u64], returns: t.ptr },
    kevy_sub_next: { args: [t.ptr, t.ptr], returns: t.i32 },
    kevy_sub_wait: { args: [t.ptr, t.u64, t.ptr], returns: t.i32 },
    kevy_sub_close: { args: [t.ptr], returns: t.void },
    kevy_version: { args: [], returns: t.cstring },
    kevy_abi: { args: [], returns: t.u32 },
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

// argv crosses the addon as one flat Buffer: a u32-LE length prefix per arg.
function nodePack(argv: Uint8Array[]): Buffer {
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
}

function nodeMakeSub(c: NapiExports, p: unknown): RawSub {
  return {
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
  };
}

function nodeMakeHandle(c: NapiExports, enc: TextEncoder, dbp: unknown): RawHandle {
  return {
    cmdRaw(argv) {
      return c.cmd(dbp, nodePack(argv));
    },
    getScalar(key) {
      const r = c.cmd(dbp, nodePack([enc.encode("GET"), key]));
      if (r == null) return null;
      // Reply is a $bulk or $-1; decode the bulk body lazily in EmbeddedDb.
      return decodeBulk(r);
    },
    setScalar(key, val, ttlMs) {
      const argv =
        ttlMs > 0
          ? [enc.encode("SET"), key, val, enc.encode("PX"), enc.encode(String(ttlMs))]
          : [enc.encode("SET"), key, val];
      c.cmd(dbp, nodePack(argv));
    },
    subscribe(name, pattern) {
      const p = pattern ? c.psubscribe(dbp, Buffer.from(name)) : c.subscribe(dbp, Buffer.from(name));
      return nodeMakeSub(c, p);
    },
    close() {
      c.close(dbp);
    },
  };
}

function loadNode(): RawEngine {
  const lib = process.env.KEVY_NAPI_LIB ?? defaultLib("libkevy_napi");
  const mod = { exports: {} as NapiExports };
  (process as unknown as { dlopen(m: object, p: string): void }).dlopen(mod, lib);
  const c = mod.exports;
  const enc = new TextEncoder();
  return {
    open(dir) {
      return nodeMakeHandle(c, enc, c.open(Buffer.from(enc.encode(dir))));
    },
    openMem() {
      return nodeMakeHandle(c, enc, c.openMem());
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
  // The length header is ASCII digits (optionally a leading '-'); hand-parse it
  // rather than spin up a TextDecoder for a handful of bytes.
  let n = 0;
  let neg = false;
  let j = 1;
  if (raw[j] === 0x2d) {
    neg = true;
    j++;
  }
  for (; j < i; j++) n = n * 10 + (raw[j]! - 0x30);
  if (neg) return null; // $-1
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

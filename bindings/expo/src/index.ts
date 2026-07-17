// expo-kevy — kevy embedded in React Native, MMKV-shaped where it counts.
//
// The native module (ios/KevyExpoModule.swift, android/…/KevyExpoModule.kt)
// is a deliberately bare shell over the same kevy-ffi stone every other
// door wraps: synchronous JSI functions, handles as small ints, argv
// packed into one flat Uint8Array, RESP bytes back. Everything typed —
// including this file's API, which mirrors @goliapkg/kevy (wasm) and
// @goliapkg/kevy-node — happens here in TypeScript.
//
//   import { open } from "expo-kevy";
//   const db = open({ dir: `${FileSystem.documentDirectory}kevy` });
//   db.set("session:7f3a", value, { ttlMs: 3_600_000 });
//   db.getText("session:7f3a");
//
// get/set ride the scalar fast path (no argv assembly, no RESP framing) —
// that is the MMKV lane. cmd() reaches all verbs; a protocol error comes
// back as a KevyError VALUE there, while typed methods throw on one.

import { requireNativeModule } from "expo-modules-core";

import { KevyError, parse, text, type KevyReply } from "./resp";

export { KevyError, text };
export type { KevyReply };

interface NativeKevy {
  open(dir: string | null): number;
  openWith(dir: string | null, opts: number[]): number;
  shutdown(id: number): void;
  close(id: number): void;
  cmd(id: number, packed: Uint8Array): Uint8Array;
  get(id: number, key: Uint8Array): Uint8Array | null;
  set(id: number, key: Uint8Array, value: Uint8Array, ttlMs: number): void;
  subscribe(id: number, chan: Uint8Array, pattern: boolean): number;
  subNext(subId: number): Uint8Array | null;
  subClose(subId: number): void;
  openReport(id: number): number[];
  version(): string;
}

const native = requireNativeModule<NativeKevy>("Kevy");

export type Bytes = string | Uint8Array;
export type MessageCallback = (payload: Uint8Array, channel: string) => void;

/** The boot-replay verdict `KevyDb.openReport()` returns. */
export interface KevyOpenStats {
  replayedCommands: number;
  replayedBytes: number;
  elapsedMs: number;
  droppedBytes: number;
  corrupt: boolean;
  quarantineCount: number;
}

export interface OpenOptions {
  /** Directory for persistence; omit for a pure in-memory store. */
  dir?: string;
  /**
   * No-op since the local fan-out lane: subscribe() callbacks are dispatched
   * a microtask after publish (there is no timer pump to pace anymore).
   * Retained so existing callers keep compiling.
   */
  tickMs?: number;
  /** AOF fsync policy: 0 everysec (default), 1 always, 2 no. */
  fsync?: number;
  /** Keyspace shards (0 = default). */
  shards?: number;
  /** Auto-rewrite growth trigger, percent (0 = rule off; default 100). */
  rewritePct?: number;
  /** Growth rule's minimum-size gate (default 64 MiB). */
  rewriteMinSize?: number;
  /** Absolute-size auto-rewrite trigger (0 = rule off). */
  rewriteBytes?: number;
  /** Staleness auto-rewrite trigger, seconds (0 = rule off). */
  rewriteIntervalSecs?: number;
}

const enc = new TextEncoder();
const dec = new TextDecoder();

// Constant verbs are re-encoded on every typed call; cache their bytes once
// (mirrors the ts client's VERB_CACHE). Only this fixed set is memoized —
// user data (keys/values) is encoded fresh and never inserted, so the map
// can't grow unbounded. The cached buffers are read-only downstream (pack
// only copies out of them), so sharing one instance across calls is safe.
const VERB_CACHE: ReadonlyMap<string, Uint8Array> = new Map(
  ["GET", "SET", "DEL", "INCRBY", "PEXPIRE", "PTTL", "KEYS", "MGET",
    "DBSIZE", "FLUSHALL", "PUBLISH", "PX"].map((w) => [w, enc.encode(w)]),
);

function toBytes(a: Bytes): Uint8Array {
  if (a instanceof Uint8Array) return a;
  return VERB_CACHE.get(a) ?? enc.encode(a);
}

// The scalar GET lane can't carry a typed store error (GET on a non-string
// key, whose only error is WRONGTYPE). Both native shells throw with this
// sentinel in the error message so get() can re-run the framed GET and
// surface the engine's -ERR WRONGTYPE — matching cmd() and every other door.
const WRONGTYPE_SIGNAL = "KEVY_WRONGTYPE";

function isWrongType(e: unknown): boolean {
  const err = e as { message?: unknown; code?: unknown } | null;
  return (
    (typeof err?.message === "string" && err.message.includes(WRONGTYPE_SIGNAL)) ||
    err?.code === "ERR_KEVY_WRONGTYPE"
  );
}

// One flat Uint8Array: u32-LE length prefix, then the bytes, per argument —
// the same packed form the JNI and N-API doors speak.
function pack(argv: Bytes[]): Uint8Array {
  const bufs = argv.map(toBytes);
  let total = 0;
  for (const b of bufs) total += 4 + b.length;
  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);
  let pos = 0;
  for (const b of bufs) {
    view.setUint32(pos, b.length, true);
    out.set(b, pos + 4);
    pos += 4 + b.length;
  }
  return out;
}

function reject(v: KevyReply): KevyReply {
  if (v instanceof KevyError) throw new Error(`kevy: ${v.message}`);
  return v;
}

class Sub {
  #id: number | null;
  constructor(id: number | null) {
    this.#id = id;
  }
  next(): KevyReply | undefined {
    if (this.#id === null) return undefined;
    const f = native.subNext(this.#id);
    return f === null ? undefined : parse(f);
  }
  close(): void {
    if (this.#id !== null) native.subClose(this.#id);
    this.#id = null;
  }
}

export class KevyDb {
  #id: number | null;
  // Same-runtime callback lane (see subscribe/publish): channel → handlers.
  // Handlers are an ARRAY, not a Set — registering the same callback twice
  // has always delivered twice, and that stays true. The snapshot array is
  // rebuilt on membership change, so a delivery already queued never sees
  // edits (snapshot-at-publish).
  #local = new Map<
    string,
    { handlers: MessageCallback[]; snapshot: MessageCallback[] | null }
  >();
  #pending: {
    receivers: MessageCallback[];
    bytes: Uint8Array;
    channel: string;
  }[] = [];
  #scheduled = false;

  constructor(id: number, _tickMs: number) {
    this.#id = id;
  }

  #live(): number {
    if (this.#id === null) throw new Error("kevy: closed handle");
    return this.#id;
  }

  // ── the escape hatch: every verb, RESP semantics, errors as values ──
  cmd(...argv: Bytes[]): KevyReply {
    return parse(native.cmd(this.#live(), pack(argv)));
  }

  // ── the MMKV lane: scalar fast path, no argv assembly, no RESP ──────
  set(key: Bytes, value: Bytes, opts: { ttlMs?: number } = {}): void {
    native.set(this.#live(), toBytes(key), toBytes(value), opts.ttlMs ?? 0);
  }

  get(key: Bytes): Uint8Array | null {
    const id = this.#live();
    try {
      return native.get(id, toBytes(key));
    } catch (e) {
      if (!isWrongType(e)) throw e;
      // The scalar lane signalled a store error (GET on a non-string key).
      // Re-run the framed GET so the engine's -ERR WRONGTYPE surfaces as a
      // thrown KevyError (via reject), exactly as cmd() and the siblings do.
      return reject(this.cmd("GET", key)) as Uint8Array | null;
    }
  }

  getText(key: Bytes): string | undefined {
    const v = this.get(key);
    return v === null ? undefined : dec.decode(v);
  }

  // ── the typed surface (mirrors the wasm and node packages) ──────────
  del(...keys: Bytes[]): number {
    return reject(this.cmd("DEL", ...keys)) as number;
  }

  incrby(key: Bytes, delta: number): number {
    return reject(this.cmd("INCRBY", key, String(delta))) as number;
  }

  expire(key: Bytes, ttlMs: number): boolean {
    return reject(this.cmd("PEXPIRE", key, String(ttlMs))) === 1;
  }

  pttl(key: Bytes): number {
    return reject(this.cmd("PTTL", key)) as number;
  }

  keys(pattern = "*", limit?: number): string[] {
    const all = (reject(this.cmd("KEYS", pattern)) as KevyReply[]).map(text) as string[];
    return limit == null ? all : all.slice(0, limit);
  }

  mget(...keys: Bytes[]): (Uint8Array | null)[] {
    return reject(this.cmd("MGET", ...keys)) as (Uint8Array | null)[];
  }

  dbsize(): number {
    return reject(this.cmd("DBSIZE")) as number;
  }

  flushall(): void {
    reject(this.cmd("FLUSHALL"));
  }

  /**
   * The boot-replay verdict: what this open restored — and what it could
   * not. `droppedBytes > 0` or `corrupt` means the store recovered LESS
   * than its files held (the dropped region was quarantined next to the
   * AOF): surface it as a startup health check instead of scraping the
   * boot WARN line from the native log.
   */
  /**
   * Flush every shard's AOF with a REAL fsync, then refuse every later
   * write (reads stay available). Idempotent — the deterministic teardown
   * for an app-lifecycle hook: `db.shutdown()` as the app backgrounds.
   */
  shutdown(): void {
    native.shutdown(this.#live());
  }

  openReport(): KevyOpenStats {
    const a = native.openReport(this.#live()) as number[];
    return {
      replayedCommands: a[0],
      replayedBytes: a[1],
      elapsedMs: a[2],
      droppedBytes: a[3],
      corrupt: a[4] !== 0,
      quarantineCount: a[5],
    };
  }

  // Publishes to the engine bus (raw-lane subscribers, receiver count) AND
  // fans out to the local callback lane in one microtask — the combined
  // count is exactly what an all-native fanout would have reported.
  publish(channel: Bytes, payload: Bytes): number {
    const engineReceivers = reject(this.cmd("PUBLISH", channel, payload)) as number;
    const name = typeof channel === "string" ? channel : dec.decode(channel);
    const ch = this.#local.get(name);
    if (ch === undefined || ch.handlers.length === 0) {
      return engineReceivers;
    }
    const receivers = ch.snapshot ?? (ch.snapshot = ch.handlers.slice());
    // Value semantics (Redis): handlers get a copy, shared per publish.
    const bytes = typeof payload === "string" ? enc.encode(payload) : payload.slice();
    this.#pending.push({ receivers, bytes, channel: name });
    if (!this.#scheduled) {
      this.#scheduled = true;
      queueMicrotask(() => this.#drain());
    }
    return engineReceivers + receivers.length;
  }

  // Callback pub/sub — the local fan-out lane, wasm-door style. An embedded
  // bus has no publisher but this handle, so a same-runtime handler needn't
  // round-trip JS→native→JS: it is dispatched IN JS, one microtask after
  // publish returns (used to be a ≤tickMs timer pump over a native sub —
  // delivery is now immediate and the idle timer is gone). Still async,
  // FIFO, at-most-once, snapshot-at-publish. The engine bus remains the
  // source of truth for the raw lanes below.
  subscribe(channel: string, cb: MessageCallback): void {
    let ch = this.#local.get(channel);
    if (ch === undefined) {
      ch = { handlers: [], snapshot: null };
      this.#local.set(channel, ch);
    }
    ch.handlers.push(cb);
    ch.snapshot = null;
  }

  subscribeRaw(channel: string): Sub {
    return new Sub(native.subscribe(this.#live(), toBytes(channel), false));
  }

  psubscribeRaw(pattern: string): Sub {
    return new Sub(native.subscribe(this.#live(), toBytes(pattern), true));
  }

  #drain(): void {
    this.#scheduled = false; // a publish inside a handler schedules anew
    const batch = this.#pending;
    this.#pending = [];
    let i = 0;
    try {
      for (; i < batch.length; i++) {
        const d = batch[i];
        for (const cb of d.receivers) {
          cb(d.bytes, d.channel);
        }
      }
    } finally {
      // A throwing handler forfeits the rest of ITS message, not the burst.
      if (i + 1 < batch.length) {
        this.#pending = batch.slice(i + 1).concat(this.#pending);
        if (!this.#scheduled) {
          this.#scheduled = true;
          queueMicrotask(() => this.#drain());
        }
      }
    }
  }

  close(): void {
    // Undelivered local deliveries drop with the handle — the same
    // at-most-once contract the old timer pump had for undrained frames.
    this.#pending = [];
    this.#local.clear();
    if (this.#id !== null) native.close(this.#id);
    this.#id = null;
  }
}

// Open the engine: { dir } for persistence, nothing for in-memory.
// Synchronous — this is React Native, the MMKV shape is the point.
//
// React Native's filesystem APIs hand back file:// URIs (expo-file-system,
// react-native-fs), so accept one: the native engine wants a plain path,
// and making every caller strip the scheme themselves is a footgun, not a
// contract. A path without a scheme passes through unchanged.
export function open(opts: OpenOptions = {}): KevyDb {
  const dir = opts.dir == null ? null : decodeURI(opts.dir).replace(/^file:\/\//, "");
  const wantsPolicy =
    opts.fsync != null || opts.shards != null || opts.rewritePct != null ||
    opts.rewriteMinSize != null || opts.rewriteBytes != null ||
    opts.rewriteIntervalSecs != null;
  if (!wantsPolicy) {
    return new KevyDb(native.open(dir), opts.tickMs ?? 50);
  }
  // The native side takes the policy as a packed number array (see the
  // module's openWith); defaults mirror plain open exactly.
  const packed = [
    opts.fsync ?? 0,
    opts.shards ?? 0,
    opts.rewritePct ?? 100,
    opts.rewriteMinSize ?? 64 * 1024 * 1024,
    opts.rewriteBytes ?? 0,
    opts.rewriteIntervalSecs ?? 0,
  ];
  return new KevyDb(native.openWith(dir, packed), opts.tickMs ?? 50);
}

export function version(): string {
  return native.version();
}

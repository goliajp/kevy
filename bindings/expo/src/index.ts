// expo-kevy — kevy embedded in React Native, MMKV-shaped where it counts.
//
// The native module (ios/KevyExpoModule.swift, android/…/KevyExpoModule.kt)
// is a deliberately bare shell over the same kevy-ffi stone every other
// door wraps: synchronous JSI functions, handles as small ints, argv
// packed into one flat Uint8Array, RESP bytes back. Everything typed —
// including this file's API, which mirrors @goliajp/kevy (wasm) and
// @goliajp/kevy-node — happens here in TypeScript.
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
  close(id: number): void;
  cmd(id: number, packed: Uint8Array): Uint8Array;
  get(id: number, key: Uint8Array): Uint8Array | null;
  set(id: number, key: Uint8Array, value: Uint8Array, ttlMs: number): void;
  subscribe(id: number, chan: Uint8Array, pattern: boolean): number;
  subNext(subId: number): Uint8Array | null;
  subClose(subId: number): void;
  version(): string;
}

const native = requireNativeModule<NativeKevy>("Kevy");

export type Bytes = string | Uint8Array;
export type MessageCallback = (payload: Uint8Array, channel: string) => void;

export interface OpenOptions {
  /** Directory for persistence; omit for a pure in-memory store. */
  dir?: string;
  /** subscribe() callback pump cadence in ms. Default 50. */
  tickMs?: number;
}

const enc = new TextEncoder();

function toBytes(a: Bytes): Uint8Array {
  return a instanceof Uint8Array ? a : enc.encode(a);
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
  constructor(private id: number | null) {}
  next(): KevyReply | undefined {
    if (this.id === null) return undefined;
    const f = native.subNext(this.id);
    return f === null ? undefined : parse(f);
  }
  close(): void {
    if (this.id !== null) native.subClose(this.id);
    this.id = null;
  }
}

export class KevyDb {
  #id: number | null;
  #subs: { sub: Sub; cb: MessageCallback }[] = [];
  #timer: ReturnType<typeof setInterval> | null = null;
  #tickMs: number;

  constructor(id: number, tickMs: number) {
    this.#id = id;
    this.#tickMs = tickMs;
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
    return native.get(this.#live(), toBytes(key));
  }

  getText(key: Bytes): string | undefined {
    const v = this.get(key);
    return v === null ? undefined : new TextDecoder().decode(v);
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

  publish(channel: Bytes, payload: Bytes): number {
    return reject(this.cmd("PUBLISH", channel, payload)) as number;
  }

  // Callback pub/sub, wasm-package style: the engine's queue is drained on
  // a timer (tickMs at open). The low-level polled handle stays available
  // as subscribeRaw() for callers who want to pump themselves.
  subscribe(channel: string, cb: MessageCallback): void {
    const sub = new Sub(native.subscribe(this.#live(), toBytes(channel), false));
    this.#subs.push({ sub, cb });
    if (!this.#timer) {
      this.#timer = setInterval(() => this.#pump(), this.#tickMs);
    }
    this.#pump();
  }

  subscribeRaw(channel: string): Sub {
    return new Sub(native.subscribe(this.#live(), toBytes(channel), false));
  }

  psubscribeRaw(pattern: string): Sub {
    return new Sub(native.subscribe(this.#live(), toBytes(pattern), true));
  }

  #pump(): void {
    for (const { sub, cb } of this.#subs) {
      for (let f = sub.next(); f !== undefined; f = sub.next()) {
        if (Array.isArray(f) && text(f[0]) === "message") {
          cb(f[2] as Uint8Array, text(f[1]) as string);
        }
      }
    }
  }

  close(): void {
    if (this.#timer) clearInterval(this.#timer);
    this.#timer = null;
    for (const { sub } of this.#subs) sub.close();
    this.#subs = [];
    if (this.#id !== null) native.close(this.#id);
    this.#id = null;
  }
}

// Open the engine: { dir } for persistence, nothing for in-memory.
// Synchronous — this is React Native, the MMKV shape is the point.
export function open(opts: OpenOptions = {}): KevyDb {
  return new KevyDb(native.open(opts.dir ?? null), opts.tickMs ?? 50);
}

export function version(): string {
  return native.version();
}

// Type definitions for @goliajp/kevy — hand-written, matching pkg/kevy.js.

/** Bytes in: strings are UTF-8 encoded; byte views pass through. */
export type Bytes = string | Uint8Array | ArrayBuffer;

export interface PersistOptions {
  /** Storage name; one log file / database per name. Default "kevy". */
  name?: string;
  /**
   * "auto" (default): OPFS via a worker, falling back to IndexedDB.
   * "opfs" / "idb" force one backend (opfs throws where unavailable).
   * localStorage is intentionally not offered: ~5 MB quota, synchronous
   * main-thread writes, and string-only storage disqualify it as a log.
   */
  backend?: "auto" | "opfs" | "idb";
}

export interface OpenOptions {
  /**
   * Module source: URL/string, ArrayBuffer, Uint8Array, Response, or a
   * compiled WebAssembly.Module. Defaults to `kevy.wasm` next to the
   * loader.
   */
  wasm?: string | URL | ArrayBuffer | Uint8Array | Response | WebAssembly.Module;
  /** Durability; omitted/false = pure in-memory. */
  persist?: false | PersistOptions;
  /** Cross-tab pub/sub bridge over BroadcastChannel. Default true. */
  broadcast?: boolean;
  /** Instance name (storage + broadcast scope). Default persist.name or "kevy". */
  name?: string;
  /** TTL sweep + event poll cadence (ms). Default 100; 0 = manual tick(). */
  tickMs?: number;
}

/** Direct-subscription callback. */
export type MessageCallback = (payload: Uint8Array, channel: string) => void;
/** Pattern-subscription callback. */
export type PatternCallback = (
  payload: Uint8Array,
  channel: string,
  pattern: string,
) => void;

/**
 * Open a kevy instance: instantiate the wasm module, replay the stored
 * log (when persistence is on), and start the tick timer and the
 * cross-tab bridge.
 */
export function open(options?: OpenOptions): Promise<Kevy>;

export class Kevy {
  /** SET; `ttlMs` adds an expiry. Throws on engine errors. */
  set(key: Bytes, value: Bytes, options?: { ttlMs?: number }): void;
  /** GET as raw bytes; undefined when absent or expired. */
  get(key: Bytes): Uint8Array | undefined;
  /** GET decoded as UTF-8 text. */
  getText(key: Bytes): string | undefined;
  /** DEL. True if the key existed. */
  del(key: Bytes): boolean;
  /** EXISTS. */
  exists(key: Bytes): boolean;
  /** PEXPIRE. True if the key exists. */
  expire(key: Bytes, ttlMs: number): boolean;
  /** PERSIST (clear TTL). True if a TTL was removed. */
  persist(key: Bytes): boolean;
  /** PTTL in ms; -1 = no TTL, -2 = no key. */
  pttl(key: Bytes): number;
  /** INCRBY. Returns the new value. */
  incrby(key: Bytes, delta?: number): number;
  /** DBSIZE. */
  dbsize(): number;
  /** FLUSHALL. */
  flushall(): void;
  /** KEYS matching a Redis glob (default all), up to limit (0 = all). */
  keys(pattern?: string, limit?: number): string[];
  /** One manual TTL sweep + event poll. Returns expired-key count. */
  tick(): number;

  /** SUBSCRIBE. Returns an unsubscribe function. */
  subscribe(channel: Bytes, cb: MessageCallback): () => void;
  /** PSUBSCRIBE with a Redis glob. Returns an unsubscribe function. */
  psubscribe(pattern: Bytes, cb: PatternCallback): () => void;
  /**
   * PUBLISH to local subscribers and (bridge on) other same-origin tabs.
   * Returns the local receiver count. Cross-tab delivery is
   * at-most-once, no backlog — only currently-open tabs receive it.
   */
  publish(channel: Bytes, payload: Bytes): number;

  /** Durability barrier: pending write frames are flushed to storage. */
  flush(): Promise<void>;
  /** Rewrite storage as a compacted image of the live keyspace. */
  compact(): Promise<void>;
  /** Flush, tear down bridge/timer/storage, free the instance. */
  close(): Promise<void>;
}

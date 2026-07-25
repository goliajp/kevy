// Type definitions for @goliapkg/kevy — hand-written, matching pkg/kevy.js.

/** Bytes in: strings are UTF-8 encoded; byte views pass through. */
export type Bytes = string | Uint8Array | ArrayBuffer;

/**
 * A decoded RESP2 reply from {@link Kevy.cmd}: simple string, integer
 * (BigInt outside the safe-integer range), bulk bytes, array, null (null
 * bulk/array), or a {@link KevyError} for a `-ERR …` reply.
 */
export type Reply =
  | string
  | number
  | bigint
  | Uint8Array
  | null
  | KevyError
  | Reply[];

/**
 * A RESP error reply (`-ERR …` / `WRONGTYPE …`) decoded by
 * {@link Kevy.cmd}. Returned, not thrown — the engine rejecting a verb is
 * data, matching the client contract's inline `Reply::Error`.
 */
export class KevyError extends Error {
  constructor(message: string);
  name: "KevyError";
}

/** Decode a bulk `Uint8Array` (or pass a string through) to text. */
export function text(v: Uint8Array | string): string;

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
  /** Read many keys in one crossing into wasm; `undefined` per miss. */
  mget(keys: Bytes[]): (Uint8Array | undefined)[];
  mgetText(keys: Bytes[]): (string | undefined)[];
  /**
   * The durable backend actually in use — where the WRITE LOG is
   * appended, not where operations are served. KV runs in wasm memory;
   * the log is batched to the backend off the hot path.
   */
  readonly backend: "opfs" | "idb" | null;
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
  /**
   * Raw command channel — run any verb the wasm build compiled in (`core`
   * + `persist`: strings/hash/list/set/zset/bitmap/keyspace/misc) and get
   * the decoded RESP2 reply. The universal escape hatch the client
   * contract mandates for verbs the typed methods do not wrap. Index
   * (`IDX.*`/`VIEW.*`) and replication verbs are not in this build and
   * return an unknown-command {@link KevyError}. Writes via `cmd` are not
   * mirrored into the persistence pump — use the typed setters for durable
   * writes.
   */
  cmd(...args: Bytes[]): Reply;
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

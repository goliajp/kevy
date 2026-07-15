// kevy-embedded: the in-process Store the mem:// / file:// URLs use
// (contract §5.2). One EmbeddedDb wraps a low-level FFI handle and parses
// raw RESP bytes into typed Reply values. This is the conformance-minimum
// embedded surface — cmd(argv), scalar get/set, and polled/blocking pub/sub
// — and is exported publicly for callers who want the store directly.

import { decodeReply, toBytes, type Data, type Reply } from "./resp.ts";
import { loadEngine, type RawHandle, type RawSub } from "./ffi.ts";
import { ClosedError } from "./errors.ts";
import { registryKey, type Target } from "./url.ts";

/** An embedded subscription handle: poll with next(), block with wait(). */
export class EmbSub {
  #raw: RawSub | null;
  constructor(raw: RawSub) {
    this.#raw = raw;
  }
  /** Drain one queued frame without blocking (null if none). */
  next(): Reply | null {
    if (!this.#raw) throw new ClosedError("subscription closed");
    const bytes = this.#raw.nextRaw();
    return bytes ? decodeReply(bytes) : null;
  }
  /** Block up to timeoutMs (0 = forever) for one frame (null on timeout). */
  wait(timeoutMs: number): Reply | null {
    if (!this.#raw) throw new ClosedError("subscription closed");
    const bytes = this.#raw.waitRaw(timeoutMs);
    return bytes ? decodeReply(bytes) : null;
  }
  close(): void {
    this.#raw?.close();
    this.#raw = null;
  }
}

/** An open in-process store (contract §5.2). Close exactly once. */
export class EmbeddedDb {
  #raw: RawHandle | null;
  private constructor(raw: RawHandle) {
    this.#raw = raw;
  }

  /** Open a persistent store rooted at dir, replaying its log on open. */
  static open(dir: string): EmbeddedDb {
    return new EmbeddedDb(loadEngine().open(dir));
  }
  /** Open a pure in-memory store — nothing survives the process. */
  static openMem(): EmbeddedDb {
    return new EmbeddedDb(loadEngine().openMem());
  }

  #handle(): RawHandle {
    if (!this.#raw) throw new ClosedError("closed handle");
    return this.#raw;
  }

  /** Run one command; argv[0] is the verb. Returns the decoded Reply — a
   * protocol -ERR arrives as a Reply of kind "error" (data, not a throw). */
  cmd(...argv: Data[]): Reply {
    const bytes = this.#handle().cmdRaw(argv.map(toBytes));
    if (!bytes) throw new ClosedError("empty reply");
    return decodeReply(bytes);
  }

  /** Scalar fast GET, or null on miss/expired (§5.2). */
  getScalar(key: Data): Uint8Array | null {
    return this.#handle().getScalar(toBytes(key));
  }
  /** Scalar fast SET (ttlMs 0 = no TTL). */
  setScalar(key: Data, val: Data, ttlMs = 0): void {
    this.#handle().setScalar(toBytes(key), toBytes(val), ttlMs);
  }

  subscribe(channel: Data): EmbSub {
    return new EmbSub(this.#handle().subscribe(toBytes(channel), false));
  }
  psubscribe(pattern: Data): EmbSub {
    return new EmbSub(this.#handle().subscribe(toBytes(pattern), true));
  }

  close(): void {
    this.#raw?.close();
    this.#raw = null;
  }

  /** Subscribe to a raw channel/pattern (used by the Subscriber layer). */
  subBytes(name: Uint8Array, pattern: boolean): EmbSub {
    return new EmbSub(this.#handle().subscribe(name, pattern));
  }
}

/** The engine version string. */
export function version(): string {
  return loadEngine().version();
}
/** The runtime C ABI version. */
export function abi(): number {
  return loadEngine().abi();
}

// --- process-global embedded registry (contract §1.3) -------------------
//
// A URL-keyed refcounted map: two connect() calls with the same mem://<name>
// or file://<path> share one store (and one pub/sub bus). The entry evicts
// when the last handle drops.

interface Shared {
  db: EmbeddedDb;
  refs: number;
}
const registry = new Map<string, Shared>();

/** Open (or share) the embedded store for a target; returns [db, key]. */
export function resolveStore(t: Target): [EmbeddedDb, string] {
  const key = registryKey(t);
  if (key !== "") {
    const s = registry.get(key);
    if (s) {
      s.refs++;
      return [s.db, key];
    }
  }
  const db = openForTarget(t);
  if (key === "") return [db, ""];
  registry.set(key, { db, refs: 1 });
  return [db, key];
}

/** Drop one reference to a shared store, closing it when the last goes. */
export function releaseStore(key: string, db: EmbeddedDb): void {
  if (key === "") {
    db.close();
    return;
  }
  const s = registry.get(key);
  if (!s) return;
  s.refs--;
  if (s.refs <= 0) {
    s.db.close();
    registry.delete(key);
  }
}

function openForTarget(t: Target): EmbeddedDb {
  if (t.kind === "file") return EmbeddedDb.open(t.path);
  return EmbeddedDb.openMem();
}

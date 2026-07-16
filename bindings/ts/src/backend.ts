// The backend abstraction behind the unified client. An embedded backend is
// synchronous in-process (so it serves both faces); a remote backend is
// async-only (remote sync is not idiomatic in JS — the sync face rejects it,
// contract §1.4 / §7). The async face wraps the same op either way, so the
// two faces agree on results.

import { toBytes, type Reply } from "./resp.ts";
import { RespConn } from "./transport.ts";
import { EmbeddedDb, releaseStore } from "./embedded.ts";
import { asOptBulk } from "./reply.ts";
import { UnsupportedError } from "./errors.ts";

export interface Backend {
  readonly isEmbedded: boolean;
  exec(argv: Uint8Array[]): Promise<Reply>;
  execSync(argv: Uint8Array[]): Reply;
  remoteConn(): RespConn;
  embDb(): EmbeddedDb;
  close(): void;
}

export class EmbeddedBackend implements Backend {
  readonly isEmbedded = true;
  #db: EmbeddedDb;
  #key: string;
  constructor(db: EmbeddedDb, key: string) {
    this.#db = db;
    this.#key = key;
  }
  async exec(argv: Uint8Array[]): Promise<Reply> {
    return this.#db.cmdBytes(argv);
  }
  execSync(argv: Uint8Array[]): Reply {
    return this.#db.cmdBytes(argv);
  }
  /** Scalar fast GET (≡ asOptBulk of a GET) — on Bun this reaches the engine's
   * shared-lane scalar symbol, skipping argv-pack + RESP encode/decode; on Node
   * the addon has no scalar symbol so this folds back to a cmd (a no-op win).
   *
   * The scalar lane cannot represent a store-semantic error: GET on a list/hash
   * key is WRONGTYPE, which the FFI collapses to an unclassifiable misuse code.
   * On that (rare, cold) signal, re-run the full GET so the -ERR maps to the
   * right StoreError (contract §2.2) — matching the generic path exactly. */
  getScalar(key: Uint8Array): Uint8Array | null {
    try {
      return this.#db.getScalar(key);
    } catch {
      return asOptBulk(this.#db.cmdBytes([toBytes("GET"), key]));
    }
  }
  /** Scalar fast SET (≡ asOK of a SET without options). */
  setScalar(key: Uint8Array, val: Uint8Array): void {
    this.#db.setScalar(key, val);
  }
  remoteConn(): never {
    throw new UnsupportedError("this feature is remote-only; use the embedded raw cmd() path or the store's typed API");
  }
  embDb(): EmbeddedDb {
    return this.#db;
  }
  close(): void {
    releaseStore(this.#key, this.#db);
  }
}

export class RemoteBackend implements Backend {
  readonly isEmbedded = false;
  #conn: RespConn;
  constructor(conn: RespConn) {
    this.#conn = conn;
  }
  exec(argv: Uint8Array[]): Promise<Reply> {
    return this.#conn.request(argv);
  }
  execSync(): never {
    throw new UnsupportedError(
      "the synchronous face is embedded-only; remote backends are async — await the Promise methods",
    );
  }
  remoteConn(): RespConn {
    return this.#conn;
  }
  embDb(): never {
    throw new UnsupportedError("not an embedded backend");
  }
  close(): void {
    this.#conn.close();
  }
}

// The backend abstraction behind the unified client. An embedded backend is
// synchronous in-process (so it serves both faces); a remote backend is
// async-only (remote sync is not idiomatic in JS — the sync face rejects it,
// contract §1.4 / §7). The async face wraps the same op either way, so the
// two faces agree on results.

import { type Reply } from "./resp.ts";
import { RespConn } from "./transport.ts";
import { EmbeddedDb, releaseStore } from "./embedded.ts";
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
    return this.#db.cmd(...argv);
  }
  execSync(argv: Uint8Array[]): Reply {
    return this.#db.cmd(...argv);
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

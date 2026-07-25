// Transactions: MULTI / EXEC / DISCARD + WATCH (contract §3.12). Remote-only
// (the embedded backend is already serial). Wire flow: MULTI → +OK; each
// queued command → +QUEUED; EXEC → array of N typed replies. A WATCH
// violation makes EXEC return Nil (aborted).
//
// JS has no deterministic destructors, so a Transaction abandoned without
// exec/discard MUST be closed explicitly (close() sends the implicit DISCARD
// so the socket isn't stuck in MULTI mode) — dropping without it is a bug.

import { replyText, replyShape, unexpectedReply, type Data, type Reply } from "./resp.ts";
import { InvalidInputError, ProtocolError } from "./errors.ts";
import { av, replyError } from "./reply.ts";
import type { RespConn } from "./transport.ts";

/** One in-flight MULTI block over a remote connection. */
export class Transaction {
  #conn: RespConn;
  #live = true;
  constructor(conn: RespConn) {
    this.#conn = conn;
  }

  /** Queue one raw command (verb + args); expects +QUEUED. */
  async queue(...parts: Data[]): Promise<this> {
    if (parts.length === 0) throw new InvalidInputError("queue needs at least a verb");
    return this.#queue(av(...parts));
  }

  set(key: Data, value: Data): Promise<this> {
    return this.#queue(av("SET", key, value));
  }
  get(key: Data): Promise<this> {
    return this.#queue(av("GET", key));
  }
  del(...keys: Data[]): Promise<this> {
    if (keys.length === 0) throw new InvalidInputError("del needs at least one key");
    return this.#queue(av("DEL", ...keys));
  }
  exists(...keys: Data[]): Promise<this> {
    if (keys.length === 0) throw new InvalidInputError("exists needs at least one key");
    return this.#queue(av("EXISTS", ...keys));
  }
  incr(key: Data): Promise<this> {
    return this.#queue(av("INCR", key));
  }
  incrBy(key: Data, delta: number): Promise<this> {
    return this.#queue(av("INCRBY", key, String(delta)));
  }
  mget(...keys: Data[]): Promise<this> {
    if (keys.length === 0) throw new InvalidInputError("mget needs at least one key");
    return this.#queue(av("MGET", ...keys));
  }
  mset(...pairs: Data[]): Promise<this> {
    if (pairs.length === 0 || pairs.length % 2 !== 0) throw new InvalidInputError("mset needs a non-empty even key/value list");
    return this.#queue(av("MSET", ...pairs));
  }

  async #queue(argv: Uint8Array[]): Promise<this> {
    const r = await this.#conn.request(argv);
    if (r.kind === "simple" && replyText(r) === "QUEUED") return this;
    if (r.kind === "error") replyError(r);
    throw unexpectedReply(r);
  }

  /** EXEC → per-command replies; a WATCH abort (Nil) collapses to []. */
  async exec(): Promise<Reply[]> {
    const r = await this.#finish();
    if (r.kind === "array") return r.items;
    if (r.kind === "nil" || r.kind === "null") return [];
    if (r.kind === "error") replyError(r);
    throw unexpectedReply(r);
  }

  /** EXEC → replies, or null when a WATCH violation aborted it. */
  async execWatched(): Promise<Reply[] | null> {
    const r = await this.#finish();
    if (r.kind === "array") return r.items;
    if (r.kind === "nil" || r.kind === "null") return null;
    if (r.kind === "error") replyError(r);
    throw unexpectedReply(r);
  }

  /** EXEC → a typed reply cursor; a WATCH abort is a Protocol error. */
  async execTyped(): Promise<TransactionReplies> {
    const r = await this.#finish();
    if (r.kind === "array") return new TransactionReplies(r.items);
    if (r.kind === "nil" || r.kind === "null") throw new ProtocolError("transaction aborted by WATCH");
    if (r.kind === "error") replyError(r);
    throw unexpectedReply(r);
  }

  /** EXEC → a typed reply cursor, or null on WATCH abort. */
  async execWatchedTyped(): Promise<TransactionReplies | null> {
    const r = await this.#finish();
    if (r.kind === "array") return new TransactionReplies(r.items);
    if (r.kind === "nil" || r.kind === "null") return null;
    if (r.kind === "error") replyError(r);
    throw unexpectedReply(r);
  }

  #finish(): Promise<Reply> {
    this.#live = false;
    return this.#conn.request(av("EXEC"));
  }

  /** Abandon the queued commands (DISCARD). */
  async discard(): Promise<void> {
    if (!this.#live) return;
    this.#live = false;
    const r = await this.#conn.request(av("DISCARD"));
    if (r.kind === "error") replyError(r);
  }

  /** Send an implicit DISCARD if still live so the socket isn't stuck in
   * MULTI mode. Idempotent — call it (or discard/exec) to avoid the leak. */
  async close(): Promise<void> {
    if (!this.#live) return;
    this.#live = false;
    await this.#conn.request(av("DISCARD")).catch(() => undefined);
  }
}

/** A typed cursor over a successful EXEC's per-command replies (§4.10). Each
 * next* consumes one reply; a mismatch is a Protocol error but the cursor
 * still advances so expectEmpty stays meaningful. */
export class TransactionReplies {
  #items: Reply[];
  #pos = 0;
  constructor(items: Reply[]) {
    this.#items = items;
  }
  remaining(): number {
    return this.#items.length - this.#pos;
  }
  expectEmpty(): void {
    const left = this.remaining();
    if (left !== 0) throw new ProtocolError(`transaction reply cursor has ${left} un-consumed replies`);
  }
  raw(): Reply {
    if (this.#pos >= this.#items.length) throw new ProtocolError("exhausted");
    return this.#items[this.#pos++]!;
  }
  nextOK(): void {
    const v = this.raw();
    if (v.kind === "simple" && replyText(v) === "OK") return;
    throw mismatch("Simple(OK)", v);
  }
  nextOKOrNil(): boolean {
    const v = this.raw();
    if (v.kind === "simple" && replyText(v) === "OK") return true;
    if (v.kind === "nil" || v.kind === "null") return false;
    throw mismatch("Simple(OK) or Nil", v);
  }
  nextInt(): number {
    const v = this.raw();
    if (v.kind === "int") return v.value;
    throw mismatch("Int", v);
  }
  nextBulk(): Uint8Array | null {
    const v = this.raw();
    if (v.kind === "bulk") return v.bytes;
    if (v.kind === "nil" || v.kind === "null") return null;
    throw mismatch("Bulk or Nil", v);
  }
  nextArrayOfBulks(): Array<Uint8Array | null> {
    const v = this.raw();
    if (v.kind === "array") {
      return v.items.map((it) => {
        if (it.kind === "bulk") return it.bytes;
        if (it.kind === "nil" || it.kind === "null") return null;
        throw mismatch("Array element Bulk/Nil", it);
      });
    }
    if (v.kind === "nil" || v.kind === "null") return [];
    throw mismatch("Array", v);
  }
  nextSimple(): Uint8Array {
    const v = this.raw();
    if (v.kind === "simple") return v.bytes;
    throw mismatch("Simple", v);
  }
}

function mismatch(want: string, got: Reply): ProtocolError {
  return new ProtocolError(`transaction reply mismatch: expected ${want}, got ${replyShape(got)}`);
}

// exported for the client's watch/unwatch/multi helpers
export async function simpleOK(conn: RespConn, argv: Uint8Array[]): Promise<void> {
  const r = await conn.request(argv);
  if (r.kind === "simple" && replyText(r) === "OK") return;
  if (r.kind === "error") replyError(r);
  throw unexpectedReply(r);
}

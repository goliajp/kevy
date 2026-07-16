// Typed-reply adapters (contract §2 / §3). Each turns a decoded Reply into
// the typed value a command promises, mapping a -ERR frame to the right
// thrown error: a recognized store-semantic error → StoreError, any other
// wire text → ProtocolError (verbatim), a shape mismatch → ProtocolError.

import {
  textOf,
  toBytes,
  unexpectedReply,
  type Data,
  type Reply,
} from "./resp.ts";
import { classifyStoreError, ProtocolError, StoreError } from "./errors.ts";

/** Build an argv (verb + args) from Data parts. */
export function av(...parts: Data[]): Uint8Array[] {
  return parts.map(toBytes);
}

/** Convert a Reply::error into the right thrown KevyError (contract §2.2). */
export function replyError(r: Reply): never {
  const text = r.kind === "error" || r.kind === "blobError" ? textOf(r.bytes) : "";
  const store = classifyStoreError(text);
  if (store) throw new StoreError(store);
  throw new ProtocolError(text);
}

function guard(r: Reply): void {
  if (r.kind === "error" || r.kind === "blobError") replyError(r);
}

/** Expect +OK. */
export function asOK(r: Reply): void {
  guard(r);
  if (r.kind === "simple" && textOf(r.bytes) === "OK") return;
  throw unexpectedReply(r);
}

/** Expect an integer. */
export function asInt(r: Reply): number {
  guard(r);
  if (r.kind === "int") return r.value;
  throw unexpectedReply(r);
}

/** Expect a non-negative count. */
export function asCount(r: Reply): number {
  const n = asInt(r);
  if (n < 0) throw new ProtocolError(`expected non-negative count, got ${n}`);
  return n;
}

/** Expect an integer, returned as a boolean (== 1). */
export function asBool(r: Reply): boolean {
  guard(r);
  if (r.kind === "int") return r.value === 1;
  throw unexpectedReply(r);
}

/** Expect a bulk or nil → bytes | null. */
export function asOptBulk(r: Reply): Uint8Array | null {
  guard(r);
  if (r.kind === "bulk") return r.bytes;
  if (r.kind === "nil" || r.kind === "null") return null;
  throw unexpectedReply(r);
}

/** Expect a simple string, returned as text (e.g. TYPE). */
export function asStr(r: Reply): string {
  guard(r);
  if (r.kind === "simple") return textOf(r.bytes);
  throw unexpectedReply(r);
}

/** Expect an array/set of bulks (no nil elements). */
export function asBulks(r: Reply): Uint8Array[] {
  guard(r);
  if (r.kind !== "array" && r.kind !== "set") throw unexpectedReply(r);
  return r.items.map((it) => {
    if (it.kind === "bulk" || it.kind === "simple") return it.bytes;
    throw unexpectedReply(it);
  });
}

/** LPOP/RPOP count-shape: array/bulk/nil → bytes[] (empty when drained). */
export function asPopList(r: Reply): Uint8Array[] {
  guard(r);
  if (r.kind === "array" || r.kind === "set") {
    return r.items.map((it) => {
      if (it.kind === "bulk" || it.kind === "simple") return it.bytes;
      throw unexpectedReply(it);
    });
  }
  if (r.kind === "bulk") return [r.bytes];
  if (r.kind === "nil" || r.kind === "null") return [];
  throw unexpectedReply(r);
}

/** Expect an array of bulk/nil (MGET-shaped). */
export function asMget(r: Reply): Array<Uint8Array | null> {
  guard(r);
  if (r.kind !== "array") throw unexpectedReply(r);
  return r.items.map((it) => {
    if (it.kind === "bulk") return it.bytes;
    if (it.kind === "nil" || it.kind === "null") return null;
    throw unexpectedReply(it);
  });
}

/** Expect an array of integers (hash field-TTL codes). */
export function asIntList(r: Reply): number[] {
  guard(r);
  if (r.kind !== "array") throw unexpectedReply(r);
  return r.items.map((it) => {
    if (it.kind === "int") return it.value;
    throw unexpectedReply(it);
  });
}

/** A float from a bulk/simple/double reply (ZSCORE / scores). */
export function scoreOf(r: Reply): number {
  if (r.kind === "double") return r.value;
  if (r.kind === "bulk" || r.kind === "simple") {
    const s = textOf(r.bytes);
    const f = Number(s);
    if (Number.isNaN(f) && s !== "nan") throw new ProtocolError(`bad float reply`);
    return f;
  }
  throw unexpectedReply(r);
}

/** Expect a double/bulk float or nil → number | null (ZSCORE). */
export function asOptDouble(r: Reply): number | null {
  guard(r);
  if (r.kind === "nil" || r.kind === "null") return null;
  return scoreOf(r);
}

/** Format a float the way the wire expects (shortest round-trip). */
export function fbytes(f: number): Uint8Array {
  return toBytes(floatStr(f));
}

/** Shortest-round-trip float string (matches Go's 'g' formatting intent). */
export function floatStr(f: number): string {
  if (f === Infinity) return "inf";
  if (f === -Infinity) return "-inf";
  return String(f);
}

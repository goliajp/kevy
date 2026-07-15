// RESP2 + RESP3 codec (contract §4.1). One decoded reply is a tagged union
// covering every wire shape; the parser is incremental (returns bytes
// consumed, 0 = need more) so the TCP transport can reassemble across reads,
// and fuzz-hardened (a hostile length header cannot force a huge alloc). The
// whole-buffer `decodeReply` is the FFI path where the full reply is present.

import { ProtocolError } from "./errors.ts";

/** A binary-safe argument: raw bytes, or a string (UTF-8 encoded on the
 * wire). The client never assumes UTF-8 for user data (contract §7). */
export type Data = Uint8Array | string;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** Encode a Data argument to wire bytes. */
export function toBytes(d: Data): Uint8Array {
  return typeof d === "string" ? encoder.encode(d) : d;
}

/** Decode bytes to a UTF-8 string (helper for string-typed values). */
export function textOf(b: Uint8Array): string {
  return decoder.decode(b);
}

/**
 * One decoded RESP value — the universal reply type covering all RESP2 and
 * RESP3 shapes (contract §4.1). Integer payloads are JS numbers (the Redis
 * ecosystem norm); values beyond 2^53 lose precision, matching node-redis.
 */
export type Reply =
  | { readonly kind: "simple"; readonly bytes: Uint8Array }
  | { readonly kind: "error"; readonly bytes: Uint8Array }
  | { readonly kind: "int"; readonly value: number }
  | { readonly kind: "bulk"; readonly bytes: Uint8Array }
  | { readonly kind: "nil" }
  | { readonly kind: "array"; readonly items: Reply[] }
  | { readonly kind: "map"; readonly pairs: ReadonlyArray<readonly [Reply, Reply]> }
  | { readonly kind: "set"; readonly items: Reply[] }
  | { readonly kind: "double"; readonly value: number }
  | { readonly kind: "boolean"; readonly value: boolean }
  | { readonly kind: "verbatim"; readonly format: string; readonly bytes: Uint8Array }
  | { readonly kind: "bigNumber"; readonly bytes: Uint8Array }
  | { readonly kind: "null" }
  | { readonly kind: "push"; readonly items: Reply[] }
  | { readonly kind: "blobError"; readonly bytes: Uint8Array };

/** Whether the reply is a protocol error (Error or BlobError). */
export function isError(r: Reply): boolean {
  return r.kind === "error" || r.kind === "blobError";
}

/** Whether the reply is a RESP2 nil or RESP3 null. */
export function isNil(r: Reply): boolean {
  return r.kind === "nil" || r.kind === "null";
}

/** The payload as a string for the byte-carrying kinds, else "". */
export function replyText(r: Reply): string {
  switch (r.kind) {
    case "simple":
    case "error":
    case "bulk":
    case "verbatim":
    case "bigNumber":
    case "blobError":
      return textOf(r.bytes);
    default:
      return "";
  }
}

/** A short name for the reply's shape, for diagnostics. */
export function replyShape(r: Reply): string {
  switch (r.kind) {
    case "simple":
      return "simple-string";
    case "bulk":
      return "bulk-string";
    case "verbatim":
      return "verbatim-string";
    case "bigNumber":
      return "big-number";
    case "blobError":
      return "blob-error";
    default:
      return r.kind;
  }
}

/** A Protocol error naming the actual variant (contract §2.2). */
export function unexpectedReply(r: Reply): ProtocolError {
  return new ProtocolError(`unexpected RESP reply variant: ${replyShape(r)}`);
}

// --- decoder ------------------------------------------------------------

/** Parse exactly one complete reply from a self-contained buffer (FFI path);
 * errors on a truncated or malformed frame. */
export function decodeReply(buf: Uint8Array): Reply {
  const [r, used] = parseReply(buf, 0);
  if (used === 0) throw new ProtocolError("truncated RESP reply");
  return r;
}

/**
 * Decode one RESP reply from buf starting at `at`. Returns [reply, endIndex]
 * where endIndex is the absolute offset past the frame; an endIndex equal to
 * `at` means "need more bytes". A malformed frame throws ProtocolError.
 * Speaks RESP2 + RESP3 and transparently skips attribute (`|`) frames.
 */
export function parseReply(buf: Uint8Array, at: number): [Reply, number] {
  if (at >= buf.length) return [NIL, at];
  const tag = buf[at]!;
  switch (tag) {
    case 0x2b: // +
      return lineReply(buf, at, "simple");
    case 0x2d: // -
      return lineReply(buf, at, "error");
    case 0x3a: // :
      return intReply(buf, at);
    case 0x24: // $
      return bulkReply(buf, at, "bulk");
    case 0x2a: // *
      return aggReply(buf, at, "array");
    case 0x25: // %
      return mapReply(buf, at);
    case 0x7e: // ~
      return aggReply(buf, at, "set");
    case 0x2c: // ,
      return doubleReply(buf, at);
    case 0x23: // #
      return boolReply(buf, at);
    case 0x3d: // =
      return verbatimReply(buf, at);
    case 0x28: // (
      return lineReply(buf, at, "bigNumber");
    case 0x5f: // _
      return nullReply(buf, at);
    case 0x3e: // >
      return aggReply(buf, at, "push");
    case 0x21: // !
      return bulkReply(buf, at, "blobError");
    case 0x7c: // |
      return attributedReply(buf, at);
    default:
      throw new ProtocolError(`unknown RESP tag 0x${tag.toString(16)}`);
  }
}

const NIL: Reply = { kind: "nil" };

function findCRLF(buf: Uint8Array, from: number): number {
  for (let i = from; i + 1 < buf.length; i++) {
    if (buf[i] === 0x0d && buf[i + 1] === 0x0a) return i;
  }
  return -1;
}

type LineKind = "simple" | "error" | "bigNumber";

function lineReply(buf: Uint8Array, at: number, kind: LineKind): [Reply, number] {
  const eol = findCRLF(buf, at + 1);
  if (eol < 0) return [NIL, at];
  return [{ kind, bytes: buf.slice(at + 1, eol) }, eol + 2];
}

function headText(buf: Uint8Array, at: number, eol: number): string {
  return textOf(buf.subarray(at + 1, eol));
}

function intReply(buf: Uint8Array, at: number): [Reply, number] {
  const eol = findCRLF(buf, at + 1);
  if (eol < 0) return [NIL, at];
  const s = headText(buf, at, eol);
  if (!/^[+-]?\d+$/.test(s)) throw new ProtocolError(`bad integer reply ${s}`);
  return [{ kind: "int", value: Number(s) }, eol + 2];
}

function bulkReply(buf: Uint8Array, at: number, kind: "bulk" | "blobError"): [Reply, number] {
  const hdr = findCRLF(buf, at + 1);
  if (hdr < 0) return [NIL, at];
  const n = parseLen(headText(buf, at, hdr), "bulk length");
  if (n < 0) return [{ kind: "nil" }, hdr + 2];
  const start = hdr + 2;
  const end = start + n;
  if (buf.length < end + 2) return [NIL, at];
  return [{ kind, bytes: buf.slice(start, end) }, end + 2];
}

function aggReply(buf: Uint8Array, at: number, kind: "array" | "set" | "push"): [Reply, number] {
  const hdr = findCRLF(buf, at + 1);
  if (hdr < 0) return [NIL, at];
  const count = parseLen(headText(buf, at, hdr), "aggregate length");
  if (count < 0) {
    if (kind === "push") throw new ProtocolError("push frame cannot be null");
    return [{ kind: "nil" }, hdr + 2];
  }
  let pos = hdr + 2;
  const items: Reply[] = [];
  for (let i = 0; i < capHint(count, buf.length - pos); i++) {
    const [item, next] = parseReply(buf, pos);
    if (next === pos) return [NIL, at];
    items.push(item);
    pos = next;
  }
  if (items.length < count) return [NIL, at];
  return [{ kind, items }, pos];
}

function mapReply(buf: Uint8Array, at: number): [Reply, number] {
  const hdr = findCRLF(buf, at + 1);
  if (hdr < 0) return [NIL, at];
  const count = parseLen(headText(buf, at, hdr), "map length");
  if (count < 0) throw new ProtocolError("bad map length");
  let pos = hdr + 2;
  const pairs: Array<readonly [Reply, Reply]> = [];
  for (let i = 0; i < capHint(count, (buf.length - pos) >> 1); i++) {
    const [k, kn] = parseReply(buf, pos);
    if (kn === pos) return [NIL, at];
    pos = kn;
    const [v, vn] = parseReply(buf, pos);
    if (vn === pos) return [NIL, at];
    pos = vn;
    pairs.push([k, v]);
  }
  if (pairs.length < count) return [NIL, at];
  return [{ kind: "map", pairs }, pos];
}

function doubleReply(buf: Uint8Array, at: number): [Reply, number] {
  const eol = findCRLF(buf, at + 1);
  if (eol < 0) return [NIL, at];
  const s = headText(buf, at, eol);
  const v = s === "inf" ? Infinity : s === "-inf" ? -Infinity : Number(s);
  if (Number.isNaN(v) && s !== "nan") throw new ProtocolError(`bad double ${s}`);
  return [{ kind: "double", value: v }, eol + 2];
}

function boolReply(buf: Uint8Array, at: number): [Reply, number] {
  const eol = findCRLF(buf, at + 1);
  if (eol < 0) return [NIL, at];
  const s = headText(buf, at, eol);
  if (s === "t") return [{ kind: "boolean", value: true }, eol + 2];
  if (s === "f") return [{ kind: "boolean", value: false }, eol + 2];
  throw new ProtocolError("bad boolean payload");
}

function verbatimReply(buf: Uint8Array, at: number): [Reply, number] {
  const hdr = findCRLF(buf, at + 1);
  if (hdr < 0) return [NIL, at];
  const n = parseLen(headText(buf, at, hdr), "verbatim length");
  if (n < 4) throw new ProtocolError("verbatim length < 4 (fmt + ':')");
  const start = hdr + 2;
  const end = start + n;
  if (buf.length < end + 2) return [NIL, at];
  const body = buf.subarray(start, end);
  if (body[3] !== 0x3a) throw new ProtocolError("verbatim missing fmt:data separator");
  return [{ kind: "verbatim", format: textOf(body.subarray(0, 3)), bytes: body.slice(4) }, end + 2];
}

function nullReply(buf: Uint8Array, at: number): [Reply, number] {
  if (buf.length < at + 3) return [NIL, at];
  if (buf[at + 1] !== 0x0d || buf[at + 2] !== 0x0a) throw new ProtocolError("bad null payload");
  return [{ kind: "null" }, at + 3];
}

// A |N attribute map decorates the reply that follows; discard it (§4.1).
function attributedReply(buf: Uint8Array, at: number): [Reply, number] {
  const [, used] = mapReply(buf, at);
  if (used === at) return [NIL, at];
  const [r, u2] = parseReply(buf, used);
  if (u2 === used) return [NIL, at];
  return [r, u2];
}

function parseLen(s: string, what: string): number {
  if (!/^-?\d+$/.test(s)) throw new ProtocolError(`bad ${what} ${s}`);
  return Number(s);
}

// Cap a header-declared count by the bytes remaining so a hostile length
// header cannot force a huge allocation (mirrors the Rust/Go parser).
function capHint(count: number, remaining: number): number {
  if (remaining < 0) remaining = 0;
  return count > remaining ? remaining : count;
}

// --- encoder ------------------------------------------------------------

/** Encode argv as a RESP multibulk request. */
export function encodeCommand(argv: Uint8Array[]): Uint8Array {
  let total = 1 + digits(argv.length) + 2;
  for (const a of argv) total += 1 + digits(a.length) + 2 + a.length + 2;
  const out = new Uint8Array(total);
  let pos = writeHeader(out, 0, 0x2a, argv.length);
  for (const a of argv) {
    pos = writeHeader(out, pos, 0x24, a.length);
    out.set(a, pos);
    pos += a.length;
    out[pos++] = 0x0d;
    out[pos++] = 0x0a;
  }
  return out;
}

function writeHeader(out: Uint8Array, pos: number, tag: number, n: number): number {
  out[pos++] = tag;
  pos += encoder.encodeInto(String(n), out.subarray(pos)).written;
  out[pos++] = 0x0d;
  out[pos++] = 0x0a;
  return pos;
}

function digits(n: number): number {
  return String(n).length;
}

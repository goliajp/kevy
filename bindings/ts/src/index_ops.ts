// Declarative secondary indexes: IDX.* (contract §3.8). Remote-only — the
// embedded backend answers Unsupported (the wire face coerces query bounds
// through the index's declared type server-side, which the client can't
// replicate without the catalog; embedded users call the store's typed API).
// Two-layered: typed shortcuts plus *Raw argv passthroughs that keep every
// server capability reachable (COMPOSE/HYBRID/GROUPS/…).

import { replyText, unexpectedReply, toBytes, type Data, type Reply } from "./resp.ts";
import { ProtocolError } from "./errors.ts";
import { type IdxType } from "./enums.ts";
import { asBool, asOK, av, replyError, scoreOf } from "./reply.ts";
import { spec } from "./specs.ts";

/** One IDX.QUERY hit: the row key + the indexed field's wire string form. */
export interface IdxRow {
  key: Uint8Array;
  value: Uint8Array;
}

/** One page of IDX.QUERY RANGE/EQ results (contract §4.4). */
export interface IdxPage {
  /** null when the scan is complete (wire "0"); else resume with it. */
  cursor: Uint8Array | null;
  rows: IdxRow[];
}

/** One IDX.LIST entry (contract §4.5); unknown labels are skipped. */
export interface IdxInfo {
  name: Uint8Array;
  prefix: Uint8Array;
  kind: string;
  state: string;
  entries: number;
  bytes: number;
}

/** One (key, score/distance) result from MATCH / KNN. */
export interface Ranked {
  key: Uint8Array;
  score: number;
}

function bulkBytes(r: Reply): Uint8Array {
  if (r.kind === "bulk" || r.kind === "simple") return r.bytes;
  throw unexpectedReply(r);
}

export function parseIdxPage(r: Reply): IdxPage {
  if (r.kind === "error") replyError(r);
  if (r.kind !== "array" || r.items.length !== 2) {
    throw new ProtocolError("IDX.QUERY page: expected [cursor, rows]");
  }
  const cur = r.items[0]!;
  let cursor: Uint8Array | null = null;
  if (cur.kind === "bulk") cursor = replyText(cur) === "0" ? null : cur.bytes;
  else throw unexpectedReply(cur);
  const flat = r.items[1]!;
  if (flat.kind !== "array") throw new ProtocolError("IDX.QUERY page: rows not an array");
  if (flat.items.length % 2 !== 0) throw new ProtocolError("IDX.QUERY page: odd row pair");
  const rows: IdxRow[] = [];
  for (let i = 0; i < flat.items.length; i += 2) {
    rows.push({ key: bulkBytes(flat.items[i]!), value: bulkBytes(flat.items[i + 1]!) });
  }
  return { cursor, rows };
}

export function parseRanked(r: Reply): Ranked[] {
  if (r.kind === "error") replyError(r);
  if (r.kind !== "array") throw unexpectedReply(r);
  return r.items.map((row) => {
    if (row.kind !== "array" || row.items.length < 2) {
      throw new ProtocolError("IDX.QUERY ranked row: expected [key, score]");
    }
    return { key: bulkBytes(row.items[0]!), score: scoreOf(row.items[1]!) };
  });
}

export function parseIdxList(r: Reply): IdxInfo[] {
  if (r.kind === "error") replyError(r);
  if (r.kind !== "array") throw unexpectedReply(r);
  return r.items.map(parseIdxInfo);
}

function parseIdxInfo(entry: Reply): IdxInfo {
  if (entry.kind !== "array") throw unexpectedReply(entry);
  const info: IdxInfo = { name: new Uint8Array(), prefix: new Uint8Array(), kind: "", state: "", entries: 0, bytes: 0 };
  const cells = entry.items;
  for (let i = 0; i + 1 < cells.length; i += 2) {
    const label = cells[i]!;
    const value = cells[i + 1]!;
    if (label.kind !== "bulk" || value.kind !== "bulk") continue;
    switch (replyText(label)) {
      case "name":
        info.name = value.bytes;
        break;
      case "prefix":
        info.prefix = value.bytes;
        break;
      case "kind":
        info.kind = replyText(value);
        break;
      case "state":
        info.state = replyText(value);
        break;
      case "entries":
        info.entries = Number(replyText(value));
        break;
      case "bytes":
        info.bytes = Number(replyText(value));
        break;
      // default: forward-compatible — skip unknown labels
    }
  }
  return info;
}

/** Serialize a query vector as an f32 little-endian blob (§7). */
export function f32Blob(vector: number[] | Float32Array): Uint8Array {
  const blob = new Uint8Array(vector.length * 4);
  const view = new DataView(blob.buffer);
  for (let i = 0; i < vector.length; i++) view.setFloat32(i * 4, vector[i]!, true);
  return blob;
}

function asRawReply(r: Reply): Reply {
  if (r.kind === "error") replyError(r);
  return r;
}

const R = true; // remoteOnly

export const idxSpecs = {
  idxCreateRange: spec(
    (name: Data, prefix: Data, field: Data, ty: IdxType) =>
      av("IDX.CREATE", name, "ON", "PREFIX", prefix, "FIELD", field, "TYPE", ty, "KIND", "range"),
    asOK,
    R,
  ),
  idxCreateRaw: spec((...args: Data[]) => av("IDX.CREATE", ...args), asOK, R),
  idxDrop: spec((name: Data) => av("IDX.DROP", name), asBool, R),
  idxList: spec(() => av("IDX.LIST"), parseIdxList, R),
  idxQueryRange: spec(
    (name: Data, min: Data, max: Data, limit: number, cursor?: Data) => {
      const parts = av("IDX.QUERY", name, "RANGE", min, max, "LIMIT", String(limit));
      if (cursor != null) parts.push(toBytes("CURSOR"), toBytes(cursor));
      return parts;
    },
    parseIdxPage,
    R,
  ),
  idxQueryEq: spec(
    (name: Data, value: Data, limit: number) => av("IDX.QUERY", name, "EQ", value, "LIMIT", String(limit)),
    parseIdxPage,
    R,
  ),
  idxQueryMatch: spec(
    (name: Data, text: Data, limit: number) => av("IDX.QUERY", name, "MATCH", text, "LIMIT", String(limit)),
    parseRanked,
    R,
  ),
  idxQueryKnn: spec(
    (name: Data, vector: number[] | Float32Array, k: number) =>
      [toBytes("IDX.QUERY"), toBytes(name), toBytes("KNN"), f32Blob(vector), toBytes("LIMIT"), toBytes(String(k))],
    parseRanked,
    R,
  ),
  idxQueryRaw: spec((...args: Data[]) => av("IDX.QUERY", ...args), asRawReply, R),
};

// Change feed / CDC: FEED.* (contract §3.10). Both backends serve the same
// cursor contract, but the pure-FFI embedded store cannot enable the feed
// (§5.1 kevy_open has no feed flag), so embedded feed_tail/read answer
// Unsupported while feed_shards is always 1. An unservable cursor surfaces as
// a Protocol error whose text starts "FEEDRESYNC <gen> <tail>" (§3.10).

import { toBytes, unexpectedReply, type Data, type Reply } from "./resp.ts";
import { InvalidInputError, ProtocolError, UnsupportedError } from "./errors.ts";
import { av, replyError } from "./reply.ts";
import type { Backend } from "./backend.ts";

/** One applied effect's argv at a stream offset (contract §4.6). */
export interface FeedFrame {
  offset: number;
  argv: Uint8Array[];
}

/** A run of frames plus the cursor to resume from (contract §4.7). */
export interface FeedBatch {
  generation: number;
  nextOffset: number;
  frames: FeedFrame[];
}

/** A (generation, nextOffset) feed cursor. */
export interface FeedCursor {
  generation: number;
  nextOffset: number;
}

const FEED_DISABLED = "feed disabled: this port's embedded connect does not enable the change feed";

export async function feedShards(be: Backend): Promise<number> {
  if (be.isEmbedded) return 1;
  const r = await be.exec(av("FEED.SHARDS"));
  return asFeedCount(r);
}

export async function feedTail(be: Backend, shard: number): Promise<FeedCursor> {
  if (be.isEmbedded) throw embeddedFeed(shard);
  return parseTail(await be.exec(av("FEED.TAIL", String(shard))));
}

export async function feedRead(
  be: Backend,
  shard: number,
  generation: number,
  offset: number,
  count: number | null,
  prefixes: Data[],
): Promise<FeedBatch> {
  if (be.isEmbedded) throw embeddedFeed(shard);
  const parts = av("FEED.READ", String(shard), String(generation), String(offset));
  if (count != null && count > 0) parts.push(toBytes("COUNT"), toBytes(String(count)));
  for (const p of prefixes) parts.push(toBytes("PREFIX"), toBytes(p));
  return parseFeedBatch(await be.exec(parts));
}

// --- sync (embedded-only) -----------------------------------------------

export function feedShardsSync(be: Backend): number {
  if (!be.isEmbedded) throw syncRemote();
  return 1;
}
export function feedTailSync(be: Backend, shard: number): FeedCursor {
  if (!be.isEmbedded) throw syncRemote();
  throw embeddedFeed(shard);
}
export function feedReadSync(be: Backend, shard: number): FeedBatch {
  if (!be.isEmbedded) throw syncRemote();
  throw embeddedFeed(shard);
}

function syncRemote(): UnsupportedError {
  return new UnsupportedError("the synchronous face is embedded-only; remote FEED.* is async — await the Promise methods");
}

function embeddedFeed(shard: number): Error {
  if (shard !== 0) return new InvalidInputError("embedded feed is single-shard: shard must be 0");
  return new UnsupportedError(FEED_DISABLED);
}

function asFeedCount(r: Reply): number {
  if (r.kind === "error") replyError(r);
  if (r.kind === "int") return r.value;
  throw unexpectedReply(r);
}

function parseTail(r: Reply): FeedCursor {
  if (r.kind === "error") replyError(r);
  if (r.kind === "array" && r.items.length === 2 && r.items[0]!.kind === "int" && r.items[1]!.kind === "int") {
    return { generation: (r.items[0] as { value: number }).value, nextOffset: (r.items[1] as { value: number }).value };
  }
  throw unexpectedReply(r);
}

function parseFeedBatch(r: Reply): FeedBatch {
  if (r.kind === "error") replyError(r);
  if (r.kind !== "array" || r.items.length !== 3) throw new ProtocolError("FEED.READ: expected [gen, next, frames]");
  const [g, next, frames] = r.items;
  if (g!.kind !== "int" || next!.kind !== "int") throw unexpectedReply(g!);
  if (frames!.kind !== "array") throw unexpectedReply(frames!);
  return {
    generation: g.value,
    nextOffset: next.value,
    frames: frames.items.map(parseFeedFrame),
  };
}

function parseFeedFrame(f: Reply): FeedFrame {
  if (f.kind !== "array" || f.items.length !== 2) throw unexpectedReply(f);
  const [off, argvRaw] = f.items;
  if (off!.kind !== "int" || argvRaw!.kind !== "array") throw unexpectedReply(f);
  const argv = argvRaw.items.map((a) => {
    if (a.kind === "bulk" || a.kind === "simple") return a.bytes;
    throw unexpectedReply(a);
  });
  return { offset: off.value, argv };
}

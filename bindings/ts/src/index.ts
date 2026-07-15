// @goliapkg/kevy — the first-party TypeScript client for kevy.
//
// One connect(url) chooses the backend from the URL scheme (contract §1):
//   mem:// / file://           → the in-process embedded engine (bun:ffi on
//                                Bun, the N-API addon on Node)
//   kevy:// / redis:// / tcp:// → a native RESP2/3 TCP client
//
// Async-by-default (Promise) — the Node/Bun idiom — with a synchronous
// embedded escape hatch on `client.sync` (contract §1.4 / §7). Runs on both
// Node and Bun with no build step.
//
//   import { connect } from "@goliapkg/kevy";
//   const c = await connect("mem://app");
//   await c.set("k", "v");
//   c.sync.get("k");            // embedded-only synchronous face

export * from "./errors.ts";
export * from "./enums.ts";
export {
  type Reply,
  type Data,
  decodeReply,
  parseReply,
  encodeCommand,
  toBytes,
  textOf,
  replyText,
  replyShape,
  isNil,
  isError,
} from "./resp.ts";

export { parseConnectURL, type Target } from "./url.ts";
export { EmbeddedDb, EmbSub, version, abi } from "./embedded.ts";

export { connect, connectSync, type Client, type SyncClient } from "./client.ts";
export { type ZMember } from "./specs.ts";
export { type KV, type ZPopHit } from "./blocking.ts";
export { type FeedFrame, type FeedBatch, type FeedCursor } from "./feed.ts";
export {
  type IdxRow,
  type IdxPage,
  type IdxInfo,
  type Ranked,
  f32Blob,
} from "./index_ops.ts";

export { Subscriber, classifyFrame, type PubsubEvent } from "./subscriber.ts";
export { Transaction, TransactionReplies } from "./transaction.ts";
export { PipelineBuf } from "./pipeline.ts";
export { ClusterClient } from "./cluster.ts";
export { crc16, keyHashSlot } from "./crc16.ts";

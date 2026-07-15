// @goliapkg/kevy — the first-party TypeScript client for kevy.
//
// One connect(url) chooses the backend from the URL scheme (contract §1):
//   mem:// / file://        → the in-process embedded engine (bun:ffi on
//                             Bun, the N-API addon on Node)
//   kevy:// / redis:// / tcp:// → a native RESP2/3 TCP client
//
// Async-by-default (Promise) — the Node/Bun idiom — with a synchronous
// embedded escape hatch on `client.sync` (contract §1.4 / §7).

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

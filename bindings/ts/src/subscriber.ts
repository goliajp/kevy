// Pub/sub consumer side (contract §3.11). A subscribed connection cannot
// send normal commands, so the consumer is a distinct type. Publishing is
// done from a normal client via publish (§3.1). Backends by URL:
// kevy:// / redis:// / tcp:// use a dedicated TCP socket; mem://<name> /
// file:// use the in-process bus. Anonymous mem:// is rejected (no other
// producer — use a named bus). recv accepts both RESP2 arrays and RESP3 push.

import { replyText, toBytes, type Data, type Reply } from "./resp.ts";
import { ClosedError, InvalidInputError, ProtocolError, TimedOutError, UnsupportedError } from "./errors.ts";
import { av, replyError } from "./reply.ts";
import { parseConnectURL, type Target } from "./url.ts";
import { RespConn } from "./transport.ts";
import { sleep } from "./ffi.ts";
import { EmbeddedDb, EmbSub, resolveStore, releaseStore } from "./embedded.ts";

/** One received pub/sub frame — acks and deliveries alike (contract §4.8).
 * Non-exhaustive: tolerate future kinds. */
export type PubsubEvent =
  | { kind: "subscribe"; channel: Uint8Array; count: number }
  | { kind: "psubscribe"; pattern: Uint8Array; count: number }
  | { kind: "unsubscribe"; channel: Uint8Array | null; count: number }
  | { kind: "punsubscribe"; pattern: Uint8Array | null; count: number }
  | { kind: "message"; channel: Uint8Array; payload: Uint8Array }
  | { kind: "pmessage"; pattern: Uint8Array; channel: Uint8Array; payload: Uint8Array };

interface EmbHandle {
  sub: EmbSub;
  name: Uint8Array;
  pattern: boolean;
}

/** A subscribed connection. */
export class Subscriber {
  #conn: RespConn | null;
  #db: EmbeddedDb | null;
  #key: string;
  #handles: EmbHandle[] = [];
  #rr = 0;
  #readTimeout: number | null = null;

  private constructor(conn: RespConn | null, db: EmbeddedDb | null, key: string) {
    this.#conn = conn;
    this.#db = db;
    this.#key = key;
  }

  /** Open a subscriber without subscribing yet. Anonymous mem:// rejected. */
  static async connect(url: string): Promise<Subscriber> {
    const t = parseConnectURL(url);
    if (t.kind === "memAnon") {
      throw new UnsupportedError("anonymous mem:// has no other producer; use mem://<name> for a shared bus");
    }
    if (t.kind === "remote") {
      return new Subscriber(await RespConn.dial(t.host, t.port), null, "");
    }
    const [db, key] = resolveStore(t as Target);
    return new Subscriber(null, db, key);
  }

  /** Open and subscribe in one step (≥1 channel required). */
  static async connectChannels(url: string, ...channels: Data[]): Promise<Subscriber> {
    if (channels.length === 0) {
      throw new InvalidInputError("connectChannels needs ≥ 1 channel — use connect for an empty start");
    }
    const s = await Subscriber.connect(url);
    try {
      await s.subscribe(...channels);
    } catch (e) {
      s.close();
      throw e;
    }
    return s;
  }

  async subscribe(...channels: Data[]): Promise<void> {
    if (channels.length === 0) throw new InvalidInputError("SUBSCRIBE needs ≥ 1 channel");
    if (this.#conn) return this.#conn.write(av("SUBSCRIBE", ...channels));
    this.#addEmb(channels, false);
  }
  async psubscribe(...patterns: Data[]): Promise<void> {
    if (patterns.length === 0) throw new InvalidInputError("PSUBSCRIBE needs ≥ 1 pattern");
    if (this.#conn) return this.#conn.write(av("PSUBSCRIBE", ...patterns));
    this.#addEmb(patterns, true);
  }
  async unsubscribe(...channels: Data[]): Promise<void> {
    if (this.#conn) return this.#conn.write(av("UNSUBSCRIBE", ...channels));
    this.#removeEmb(channels.map(toBytes), false);
  }
  async punsubscribe(...patterns: Data[]): Promise<void> {
    if (this.#conn) return this.#conn.write(av("PUNSUBSCRIBE", ...patterns));
    this.#removeEmb(patterns.map(toBytes), true);
  }

  /** Negotiate RESP3 push frames; remote-only, must precede any subscribe. */
  async hello3(): Promise<PubsubEvent> {
    if (!this.#conn) throw new UnsupportedError("HELLO 3 is remote/TCP-only; the embedded bus has no proto switch");
    this.#conn.write(av("HELLO", "3"));
    const r = await this.#conn.readReply();
    if (r.kind === "map" || r.kind === "array") return { kind: "subscribe", channel: toBytes("HELLO"), count: 3 };
    if (r.kind === "error") replyError(r);
    throw new ProtocolError(`unexpected HELLO 3 reply shape: ${r.kind}`);
  }

  /** Block for the next pub/sub frame (acks and deliveries). */
  async recv(): Promise<PubsubEvent> {
    if (this.#conn) return classifyFrame(await this.#conn.readReply());
    return this.#recvEmb();
  }

  /** Block for the next Message/Pmessage, skipping ack frames. */
  async recvMessage(): Promise<[Uint8Array, Uint8Array]> {
    for (;;) {
      const ev = await this.recv();
      if (ev.kind === "message") return [ev.channel, ev.payload];
      if (ev.kind === "pmessage") return [ev.channel, ev.payload];
    }
  }

  /** Bound a blocking recv (null clears it). Timeout surfaces as TimedOut. */
  setReadTimeout(ms: number | null): void {
    this.#readTimeout = ms;
    this.#conn?.setReadTimeout(ms);
  }

  /** An async iterator over events; terminates when the connection closes. */
  async *events(): AsyncGenerator<PubsubEvent> {
    for (;;) {
      try {
        yield await this.recv();
      } catch (e) {
        if (e instanceof ClosedError) return;
        throw e;
      }
    }
  }

  /** An async iterator over (channel, payload) deliveries (acks skipped). */
  async *messages(): AsyncGenerator<[Uint8Array, Uint8Array]> {
    for (;;) {
      try {
        yield await this.recvMessage();
      } catch (e) {
        if (e instanceof ClosedError) return;
        throw e;
      }
    }
  }

  close(): void {
    this.#conn?.close();
    this.#conn = null;
    for (const h of this.#handles) h.sub.close();
    this.#handles = [];
    if (this.#db) {
      releaseStore(this.#key, this.#db);
      this.#db = null;
    }
  }

  // --- embedded bus internals -------------------------------------------

  #addEmb(names: Data[], pattern: boolean): void {
    const db = this.#db!;
    for (const n of names) {
      const nb = toBytes(n);
      this.#handles.push({ sub: db.subBytes(nb, pattern), name: nb, pattern });
    }
  }

  #removeEmb(names: Uint8Array[], pattern: boolean): void {
    const keep: EmbHandle[] = [];
    for (const h of this.#handles) {
      const drop = h.pattern === pattern && (names.length === 0 || names.some((m) => eq(m, h.name)));
      if (drop) h.sub.close();
      else keep.push(h);
    }
    this.#handles = keep;
  }

  async #recvEmb(): Promise<PubsubEvent> {
    const end = this.#readTimeout == null ? null : Date.now() + this.#readTimeout;
    for (;;) {
      const n = this.#handles.length;
      for (let i = 0; i < n; i++) {
        const h = this.#handles[(this.#rr + i) % n]!;
        const frame = h.sub.next();
        if (frame) {
          this.#rr = (this.#rr + i + 1) % n;
          return classifyFrame(frame);
        }
      }
      if (end != null && Date.now() >= end) throw new TimedOutError();
      await sleep(2);
    }
  }
}

function eq(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

/** Shape a RESP2 array or RESP3 push frame into a PubsubEvent (§3.11). */
export function classifyFrame(r: Reply): PubsubEvent {
  if (r.kind === "error") replyError(r);
  const items = r.kind === "array" || r.kind === "push" ? r.items : null;
  if (!items || items.length === 0 || items[0]!.kind !== "bulk") {
    throw new ProtocolError(`expected pub/sub frame, got ${r.kind}`);
  }
  const kind = replyText(items[0]!);
  switch (kind) {
    case "subscribe":
      return { kind: "subscribe", channel: ackName(items), count: ackCount(items) };
    case "psubscribe":
      return { kind: "psubscribe", pattern: ackName(items)!, count: ackCount(items) };
    case "unsubscribe":
      return { kind: "unsubscribe", channel: ackNameOpt(items), count: ackCount(items) };
    case "punsubscribe":
      return { kind: "punsubscribe", pattern: ackNameOpt(items), count: ackCount(items) };
    case "message":
      if (items.length !== 3 || items[1]!.kind !== "bulk" || items[2]!.kind !== "bulk") throw new ProtocolError("bad message frame");
      return { kind: "message", channel: items[1]!.bytes, payload: items[2]!.bytes };
    case "pmessage":
      if (items.length !== 4 || items[1]!.kind !== "bulk" || items[2]!.kind !== "bulk" || items[3]!.kind !== "bulk") {
        throw new ProtocolError("bad pmessage frame");
      }
      return { kind: "pmessage", pattern: items[1]!.bytes, channel: items[2]!.bytes, payload: items[3]!.bytes };
    default:
      throw new ProtocolError(`unknown pubsub kind '${kind}'`);
  }
}

function ackName(items: Reply[]): Uint8Array {
  const n = ackNameOpt(items);
  if (!n) throw new ProtocolError("pubsub ack missing name");
  return n;
}
function ackNameOpt(items: Reply[]): Uint8Array | null {
  if (items.length !== 3) throw new ProtocolError(`expected 3-element pubsub frame, got ${items.length}`);
  const name = items[1]!;
  if (name.kind === "bulk") return name.bytes;
  if (name.kind === "nil" || name.kind === "null") return null;
  throw new ProtocolError("bad pubsub name field");
}
function ackCount(items: Reply[]): number {
  const c = items[2]!;
  if (c.kind !== "int") throw new ProtocolError("bad pubsub count field");
  return c.value;
}

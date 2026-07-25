// tauri-plugin-kevy — guest (webview) binding.
//
// A typed client wrapping `invoke('plugin:kevy|<cmd>', …)` into the kevy client
// families (docs/client-contract.md §3). ONE shared store lives in the Rust
// backend; every call here reaches it. Keys/values accept `string` or bytes and
// are packed to number arrays (binary-safe); replies come back the same way and
// are decoded by the `reply` helpers.

import { invoke, Channel } from '@tauri-apps/api/core'
import {
  Reply,
  toByteArray,
  replyBytes,
  replyString,
  replyInt,
  replyBytesList,
} from './reply'
import { PubsubMsg, Subscription } from './pubsub'

export * from './reply'
export * from './pubsub'

const P = 'plugin:kevy|'

/** A key/value/channel argument: a UTF-8 string or raw bytes. */
export type BytesIn = string | Uint8Array | ArrayBuffer | number[]

/** Options for {@link KevyClient.set}. */
export interface SetOptions {
  /** Expiry in milliseconds (`SET … PX ttlMs`). Omit / `0` for no expiry. */
  ttlMs?: number
}

/** Options for {@link KevyClient.subscribe}. At least one is required. */
export interface SubscribeOptions {
  /** Exact channels to subscribe to. */
  channels?: BytesIn[]
  /** Glob patterns to subscribe to (`*`, `?`, `[…]`). */
  patterns?: BytesIn[]
}

/**
 * The webview-side kevy client. Construct one and call methods, or use the
 * shared {@link kevy} singleton.
 */
export class KevyClient {
  // ── Raw escape hatch ────────────────────────────────────────────────────
  /** Run any kevy verb by argv; get the decoded RESP {@link Reply}. */
  cmd(argv: BytesIn[]): Promise<Reply> {
    return invoke<Reply>(`${P}cmd`, { argv: argv.map(toByteArray) })
  }

  // ── Core string / generic key (§3.1) ────────────────────────────────────
  /** `GET key` — raw bytes, or `null` when absent/expired. */
  get(key: BytesIn): Promise<number[] | null> {
    return invoke<number[] | null>(`${P}get`, { key: toByteArray(key) })
  }

  /** `GET key` decoded as a UTF-8 string, or `null`. */
  async getString(key: BytesIn): Promise<string | null> {
    const bytes = await this.get(key)
    return bytes === null ? null : new TextDecoder().decode(Uint8Array.from(bytes))
  }

  /** `SET key value [PX ttlMs]`. */
  set(key: BytesIn, value: BytesIn, opts: SetOptions = {}): Promise<void> {
    return invoke(`${P}set`, {
      key: toByteArray(key),
      value: toByteArray(value),
      ttlMs: opts.ttlMs ?? null,
    })
  }

  /** `DEL key [key …]` — number removed. */
  del(...keys: BytesIn[]): Promise<number> {
    return invoke<number>(`${P}del`, { keys: keys.map(toByteArray) })
  }

  /** `EXISTS key [key …]` — a repeated key counts each time. */
  exists(...keys: BytesIn[]): Promise<number> {
    return invoke<number>(`${P}exists`, { keys: keys.map(toByteArray) })
  }

  /** `INCR key`, or `INCRBY key by` when `by` is given. */
  incr(key: BytesIn, by?: number): Promise<number> {
    return invoke<number>(`${P}incr`, { key: toByteArray(key), by: by ?? null })
  }

  /** `PING` — resolves on the embedded backend. */
  ping(): Promise<void> {
    return invoke(`${P}ping`)
  }

  /** `DBSIZE` — live keys at call time. */
  dbsize(): Promise<number> {
    return invoke<number>(`${P}dbsize`)
  }

  /** `FLUSHALL` — wipe the store. */
  flushall(): Promise<void> {
    return invoke(`${P}flushall`)
  }

  // ── Pub/sub (§3.11) ─────────────────────────────────────────────────────
  /** `PUBLISH channel payload` — in-process subscribers reached. */
  publish(channel: BytesIn, payload: BytesIn): Promise<number> {
    return invoke<number>(`${P}publish`, {
      channel: toByteArray(channel),
      payload: toByteArray(payload),
    })
  }

  /**
   * Subscribe to `channels` and/or `patterns`; `onEvent` fires for every frame
   * (acks + deliveries). Returns a {@link Subscription} — call `.unsubscribe()`
   * to stop the backend poller.
   */
  async subscribe(
    opts: SubscribeOptions,
    onEvent: (msg: PubsubMsg) => void,
  ): Promise<Subscription> {
    const ch = new Channel<PubsubMsg>()
    ch.onmessage = onEvent
    const id = await invoke<number>(`${P}subscribe`, {
      channels: (opts.channels ?? []).map(toByteArray),
      patterns: (opts.patterns ?? []).map(toByteArray),
      onEvent: ch,
    })
    return new Subscription(id)
  }

  // ── Typed families over the raw path (§3.2–3.5) ─────────────────────────
  // These show the raw-argv escape hatch producing typed results; extend as
  // needed — every verb is reachable through `cmd`.

  /** `HSET key field value [field value …]` — newly-added field count. */
  async hset(key: BytesIn, ...pairs: BytesIn[]): Promise<number> {
    return replyInt(await this.cmd(['HSET', key, ...pairs]))
  }

  /** `HGET key field` — value bytes, or `null`. */
  async hget(key: BytesIn, field: BytesIn): Promise<Uint8Array | null> {
    return replyBytes(await this.cmd(['HGET', key, field]))
  }

  /** `LPUSH key value [value …]` — new length. */
  async lpush(key: BytesIn, ...values: BytesIn[]): Promise<number> {
    return replyInt(await this.cmd(['LPUSH', key, ...values]))
  }

  /** `RPUSH key value [value …]` — new length. */
  async rpush(key: BytesIn, ...values: BytesIn[]): Promise<number> {
    return replyInt(await this.cmd(['RPUSH', key, ...values]))
  }

  /** `LRANGE key start stop` — element bytes. */
  async lrange(key: BytesIn, start: number, stop: number): Promise<(Uint8Array | null)[]> {
    return replyBytesList(await this.cmd(['LRANGE', key, String(start), String(stop)]))
  }

  /** `SADD key member [member …]` — newly-added count. */
  async sadd(key: BytesIn, ...members: BytesIn[]): Promise<number> {
    return replyInt(await this.cmd(['SADD', key, ...members]))
  }

  /** `SMEMBERS key` — member bytes. */
  async smembers(key: BytesIn): Promise<(Uint8Array | null)[]> {
    return replyBytesList(await this.cmd(['SMEMBERS', key]))
  }

  /** `ZADD key score member [score member …]` — newly-added count. */
  async zadd(key: BytesIn, ...scoreMember: (BytesIn | number)[]): Promise<number> {
    const argv: BytesIn[] = ['ZADD', key, ...scoreMember.map((x) => (typeof x === 'number' ? String(x) : x))]
    return replyInt(await this.cmd(argv))
  }

  /** `ZRANGE key start stop` — member bytes (ascending score). */
  async zrange(key: BytesIn, start: number, stop: number): Promise<(Uint8Array | null)[]> {
    return replyBytesList(await this.cmd(['ZRANGE', key, String(start), String(stop)]))
  }
}

/** A ready-to-use client bound to the app's kevy plugin. */
export const kevy = new KevyClient()

// Re-export the decode helpers callers reach for on `cmd` replies.
export { replyString, replyBytes, replyInt }

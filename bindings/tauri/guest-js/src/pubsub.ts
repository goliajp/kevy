// Pub/sub types + the subscription handle.
//
// A `subscribe` opens a Tauri Channel; the Rust plugin's poller thread streams
// frames into it (see tauri-plugin-kevy/src/pubsub.rs). Mirrors the
// `PubsubEvent` taxonomy in docs/client-contract.md §4.8. Channel/payload bytes
// arrive as JSON number arrays (binary-safe).

import { invoke } from '@tauri-apps/api/core'
import type { ByteArray } from './reply'

/** A pub/sub frame delivered to the webview, tagged by `kind`. */
export type PubsubMsg =
  | { kind: 'subscribe'; channel: ByteArray; count: number }
  | { kind: 'psubscribe'; pattern: ByteArray; count: number }
  | { kind: 'unsubscribe'; channel: ByteArray | null; count: number }
  | { kind: 'punsubscribe'; pattern: ByteArray | null; count: number }
  | { kind: 'message'; channel: ByteArray; payload: ByteArray }
  | { kind: 'pmessage'; pattern: ByteArray; channel: ByteArray; payload: ByteArray }

const dec = new TextDecoder()

/** A message frame's channel bytes as a string (for `message`/`pmessage`). */
export function msgChannel(m: PubsubMsg): string | null {
  return 'channel' in m && m.channel ? dec.decode(Uint8Array.from(m.channel)) : null
}

/** A message frame's payload as a `Uint8Array` (for `message`/`pmessage`). */
export function msgPayload(m: PubsubMsg): Uint8Array | null {
  return 'payload' in m ? Uint8Array.from(m.payload) : null
}

/** A message frame's payload as a UTF-8 string (for `message`/`pmessage`). */
export function msgPayloadString(m: PubsubMsg): string | null {
  const p = msgPayload(m)
  return p === null ? null : dec.decode(p)
}

/**
 * A live subscription. Call {@link unsubscribe} to stop the backend poller and
 * free its thread. Dropping the JS handle alone does NOT stop the poller — the
 * store lives in Rust — so always `unsubscribe()` on teardown.
 */
export class Subscription {
  constructor(private readonly id: number) {}

  /** Stop the backend poller. Returns whether it was still running. */
  async unsubscribe(): Promise<boolean> {
    return invoke<boolean>('plugin:kevy|unsubscribe', { id: this.id })
  }
}

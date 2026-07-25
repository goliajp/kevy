import { NitroModules } from 'react-native-nitro-modules'
import type { KevyNitro } from './specs/KevyNitro.nitro'

// react-native-kevy-nitro — kevy in React Native over a Nitro (JSI) C++
// HybridObject. The lanes, in order of what you reach for:
//
//   1. getData/setData — the scalar KV door (the MMKV-shaped lane). Raw
//      key/value ArrayBuffers straight into kevy_get/kevy_set: no argv
//      packing, no RESP framing, no JS decode. The fastest KV path.
//   2. cmd(argv) — the general fast path. One synchronous JSI call that runs
//      any kevy verb and returns the RESP reply as an ArrayBuffer.
//   3. subscribe/publish/subNext — the default pub/sub. Poll-drain on the
//      JS thread, no background thread, no CPU when you're not calling it.
//   4. subscribePushBatched — the high-fanout path. A native poller parks
//      on the engine (kevy_sub_wait_raw, zero idle CPU) and delivers each burst
//      of frames in ONE native->JS hop, packed into ONE ArrayBuffer. Prefer
//      this over subscribePush (per-message): the per-frame CallInvoker hop
//      costs more than it saves.
//
// Always stopPush() a push subscription (or drop the object) to join the
// poller thread. The db handle lives on the native side; nothing but the
// ArrayBuffer payloads crosses the bridge.
export function createKevyNitro(): KevyNitro {
  return NitroModules.createHybridObject<KevyNitro>('KevyNitro')
}

// Durable variant: open the db file-backed at `dir` (AOF, replayed on open)
// instead of in-memory. Use when you need persistence across app launches —
// the fair shape against MMKV. In-memory (createKevyNitro) is faster but
// ephemeral. Throws if the durable store cannot be opened at `dir`.
export function createKevyNitroAt(dir: string): KevyNitro {
  const kevy = NitroModules.createHybridObject<KevyNitro>('KevyNitro')
  if (!kevy.openAt(dir)) {
    throw new Error(`kevy: failed to open durable store at "${dir}"`)
  }
  return kevy
}

// ── local fan-out lane (the hybrid bus) ───────────────────────────────────
//
// Same-runtime pub/sub without the crossing tax on DELIVERY. The engine's
// bus is in-process and this door's raw lanes all round-trip JS→native→JS,
// so for subscribers living in this same JS runtime the native leg is pure
// transport overhead (measured: the whole round-trip trails a plain JS
// emitter ~6-10× on device). The bus lane puts local subscribers where a JS
// emitter physically sits — dispatched in JS, no crossing — while every
// publish STILL goes to the engine bus, which stays the source of truth for
// the raw lanes (subscribe/subNext, subscribePush*) and the receiver count.
// The wasm door's cross-tab bridge is the same architecture (engine bus +
// binding-layer transport, honestly documented).
//
// Semantics vs the raw lanes (Redis-compatible on every axis):
//   - a handler subscribed here is dispatched ONLY here (never also via the
//     native lanes — no double delivery, by construction);
//   - delivery is async (a microtask queued at publish; publish returns
//     first), FIFO per subscriber, at-most-once, no backlog — the same
//     contract as the engine bus;
//   - the delivery set is snapshotted at publish time (Redis: receivers are
//     those subscribed when PUBLISH ran);
//   - handlers get a COPY of the payload (Redis value semantics; one copy
//     shared by all handlers of that publish — don't mutate it);
//   - publish returns engine receivers + local receivers, the same total a
//     pure-native fanout would have reported.
export type KevyBusHandler = (payload: Uint8Array, channel: string) => void

export interface KevyBus {
  // Register a local handler; returns its unsubscribe function.
  subscribe(channel: string, handler: KevyBusHandler): () => void
  // Publish to the engine bus AND the local handlers; returns the combined
  // receiver count.
  publish(channel: string, payload: ArrayBuffer): number
}

type KevyBusDelivery = {
  receivers: KevyBusHandler[]
  bytes: Uint8Array
  channel: string
}

// Per-channel registry entry: the Set is membership truth, the array is the
// publish-time snapshot, rebuilt lazily after every (un)subscribe. Publishing
// reuses the SAME array until membership changes — mutations always build a
// fresh array, so a snapshot already queued for delivery is never edited
// (snapshot-at-publish holds). A Set spread per publish was measured at ~1/3
// of the whole publish cost on device (Hermes iterator protocol).
type KevyBusChannel = {
  handlers: Set<KevyBusHandler>
  snapshot: KevyBusHandler[] | null
}

export function createKevyBus(nitro: KevyNitro): KevyBus {
  const local = new Map<string, KevyBusChannel>()
  // One microtask drains the whole publish burst. Per-publish scheduling (a
  // fresh closure + a queueMicrotask each) measured ~4× the entire native
  // publish leg on device — the queue entry below is a push and a flag test.
  // Semantics are unchanged: still async (nothing runs before the publishing
  // code yields), still FIFO, still snapshot-at-publish per delivery.
  let pending: KevyBusDelivery[] = []
  let scheduled = false
  const drain = () => {
    scheduled = false // a publish from inside a handler schedules a new drain
    const batch = pending
    pending = []
    let i = 0
    try {
      for (; i < batch.length; i++) {
        const d = batch[i]
        for (const handler of d.receivers) {
          handler(d.bytes, d.channel)
        }
      }
    } finally {
      // A throwing handler forfeits the rest of ITS message (an emitter's
      // contract) but must not take the rest of the burst down with it —
      // per-message isolation is what per-publish microtasks gave us.
      if (i + 1 < batch.length) {
        pending = batch.slice(i + 1).concat(pending)
        if (!scheduled) {
          scheduled = true
          queueMicrotask(drain)
        }
      }
    }
  }
  return {
    subscribe(channel, handler) {
      let ch = local.get(channel)
      if (ch === undefined) {
        ch = { handlers: new Set(), snapshot: null }
        local.set(channel, ch)
      }
      ch.handlers.add(handler)
      ch.snapshot = null
      return () => {
        ch.handlers.delete(handler)
        ch.snapshot = null
        if (ch.handlers.size === 0) local.delete(channel)
      }
    },
    publish(channel, payload) {
      const engineReceivers = nitro.publish(channel, payload)
      const ch = local.get(channel)
      if (ch === undefined || ch.handlers.size === 0) {
        return engineReceivers
      }
      const receivers = ch.snapshot ?? (ch.snapshot = [...ch.handlers])
      pending.push({
        receivers, // the current-membership snapshot (see KevyBusChannel)
        bytes: new Uint8Array(payload.slice(0)), // value semantics
        channel,
      })
      if (!scheduled) {
        scheduled = true
        queueMicrotask(drain)
      }
      return engineReceivers + receivers.length
    },
  }
}

// Unpack a subscribePushBatched buffer into per-frame Uint8Array views. The
// packed buffer is [u32-LE length][bytes] repeated `count` times; each view
// is a zero-copy `subarray` over the shared buffer (a pointer+len, no copy).
export function unpackFrames(packed: ArrayBuffer, count: number): Uint8Array[] {
  const bytes = new Uint8Array(packed)
  const view = new DataView(packed)
  const frames: Uint8Array[] = new Array(count)
  let pos = 0
  for (let i = 0; i < count; i++) {
    const len = view.getUint32(pos, true)
    pos += 4
    frames[i] = bytes.subarray(pos, pos + len) // zero-copy view
    pos += len
  }
  return frames
}

export type { KevyNitro }
export type { KevyOpenStats } from './specs/KevyNitro.nitro'

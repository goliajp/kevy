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
//      on the engine (kevy_sub_wait, zero idle CPU) and delivers each burst
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

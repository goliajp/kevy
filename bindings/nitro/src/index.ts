import { NitroModules } from 'react-native-nitro-modules'
import type { KevyNitro } from './specs/KevyNitro.nitro'

// react-native-kevy-nitro — kevy in React Native over a Nitro (JSI) C++
// HybridObject. Three lanes, in order of what you reach for:
//
//   1. cmd(argv) — the fast path. One synchronous JSI call into C++ that
//      runs any kevy verb and returns the RESP reply as an ArrayBuffer.
//      ~3x the Expo door on PING; this is the store/KV hot path.
//   2. subscribe/publish/subNext — the default pub/sub. Poll-drain on the
//      JS thread, no background thread, no CPU when you're not calling it.
//   3. subscribePushBatched — the high-fanout path. A native poller parks
//      on the engine (kevy_sub_wait, zero idle CPU) and delivers each burst
//      of frames in ONE native->JS hop. Beats poll ~1.3x on throughput.
//      Prefer this over subscribePush (per-message): the per-frame
//      CallInvoker hop costs more than it saves.
//
// Always stopPush() a push subscription (or drop the object) to join the
// poller thread. The db handle lives on the native side; nothing but the
// ArrayBuffer payloads crosses the bridge.
export function createKevyNitro(): KevyNitro {
  return NitroModules.createHybridObject<KevyNitro>('KevyNitro')
}

export type { KevyNitro }

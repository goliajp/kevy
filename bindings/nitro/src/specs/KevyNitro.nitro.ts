import type { HybridObject } from 'react-native-nitro-modules'

// A C++-only Nitro HybridObject: JS calls these methods *directly via JSI*
// into the C++ class, which calls kevy-ffi (the prebuilt C ABI). No Expo
// module dispatch, no packed-argv marshalling through JNI, no Kotlin hop —
// the JS<->native crossing collapses to a JSI HostFunction call.
//
// The object owns one in-memory kevy db (opened in its C++ constructor),
// one raw poll subscription, and one push subscription (a native poller
// thread that hops frames onto the JS thread).
export interface KevyNitro
  extends HybridObject<{ ios: 'c++'; android: 'c++' }> {
  // kevy_abi() — the trivial crossing, isolates pure JSI dispatch cost.
  abi(): number

  // One command. argv is the same packed form the JNI/N-API doors speak
  // (u32-LE length prefix per arg, then bytes), passed as an ArrayBuffer by
  // reference. Returns the RESP reply bytes as an ArrayBuffer.
  cmd(argv: ArrayBuffer): ArrayBuffer

  // Poll-model pub/sub (bonus), mirroring kevy-ffi's polled sub. One
  // subscription per object; subNext drains one RESP frame or undefined.
  subscribe(channel: string): void
  publish(channel: string, payload: ArrayBuffer): void
  subNext(): ArrayBuffer | undefined

  // Push-model pub/sub. A dedicated NATIVE poller thread blocks in
  // kevy_sub_wait — a real kernel park on the engine's mpsc channel, so it
  // burns 0% CPU while idle (no busy-spin) — and, per frame, invokes this JS
  // callback, which Nitro auto-hops onto the JS thread (AsyncJSCallback ->
  // CallInvoker). That is JS-side push (one callback per message, zero
  // JS-side polling) with a native poller that sleeps until a frame arrives.
  //
  // Per-message: one native->JS hop per frame.
  subscribePush(channel: string, onMessage: (frame: ArrayBuffer) => void): void
  // Batched: the poller drains ALL available frames per wake and delivers
  // them in ONE hop — amortizes the CallInvoker hop across a batch.
  subscribePushBatched(
    channel: string,
    onBatch: (frames: ArrayBuffer[]) => void
  ): void
  // Stop the poller thread and close the push subscription.
  stopPush(): void
}

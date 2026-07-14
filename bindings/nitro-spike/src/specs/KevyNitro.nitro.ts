import type { HybridObject } from 'react-native-nitro-modules'

// A C++-only Nitro HybridObject: JS calls these methods *directly via JSI*
// into the C++ class, which calls kevy-ffi (the prebuilt C ABI). No Expo
// module dispatch, no packed-argv marshalling through JNI, no Kotlin hop —
// the whole point of the spike is to measure that crossing removed.
//
// The object owns one in-memory kevy db (opened in its C++ constructor)
// plus, optionally, one raw subscription — enough to bench cmd() round
// trips and a poll-model pub/sub against the current Expo door.
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
}

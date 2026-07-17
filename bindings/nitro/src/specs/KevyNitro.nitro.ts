import type { HybridObject } from 'react-native-nitro-modules'

// A C++-only Nitro HybridObject: JS calls these methods *directly via JSI*
// into the C++ class, which calls kevy-ffi (the prebuilt C ABI). No Expo
// module dispatch, no packed-argv marshalling through JNI, no Kotlin hop —
// the JS<->native crossing collapses to a JSI HostFunction call.
//
// The object owns one in-memory kevy db (opened in its C++ constructor),
// one raw poll subscription, and one push subscription (a native poller
// thread that hops frames onto the JS thread).
// The boot-replay verdict openReport() returns: droppedBytes > 0 or corrupt
// means the store recovered LESS than its files held (the dropped region was
// quarantined next to the AOF) — surface it as a startup health check.
export interface KevyOpenStats {
  replayedCommands: number
  replayedBytes: number
  elapsedMs: number
  droppedBytes: number
  corrupt: boolean
  quarantineCount: number
}

export interface KevyNitro
  extends HybridObject<{ ios: 'c++'; android: 'c++' }> {
  // kevy_abi() — the trivial crossing, isolates pure JSI dispatch cost.
  abi(): number

  // One command. argv is the same packed form the JNI/N-API doors speak
  // (u32-LE length prefix per arg, then bytes), passed as an ArrayBuffer by
  // reference. Returns the RESP reply bytes as an ArrayBuffer.
  cmd(argv: ArrayBuffer): ArrayBuffer

  // Scalar KV door — the MMKV-shaped fast lane. get/set with NO argv packing,
  // NO RESP framing, NO verb. Calls kevy_get / kevy_set directly, so the JS
  // side hands raw key/value ArrayBuffers and gets raw value bytes back. This
  // removes the whole RESP tax (JS packAB + C++ argv unpack + engine RESP
  // encode + reply framing + JS RESP decode) — only the one JSI hop and the
  // one unavoidable value copy each way remain.
  //   getData: key string -> value ArrayBuffer, or undefined on miss.
  //   setData: key string + value ArrayBuffer, ttlMs (0 = no TTL); returns
  //            void — no reply is framed or crossed at all.
  // The key is a string (not an ArrayBuffer): a short UTF-8 key marshals far
  // cheaper across JSI than an ArrayBuffer arg (measured ~560 ns/AB-arg on
  // device), and it mirrors MMKV's string-key API. The value stays an
  // ArrayBuffer (binary-safe, arbitrary bytes).
  getData(key: string): ArrayBuffer | undefined
  setData(key: string, value: ArrayBuffer, ttlMs: number): void

  // The boot-replay verdict of this instance's open (all zeros for the
  // in-memory constructor db; meaningful after openAt).
  openReport(): KevyOpenStats

  // Re-open this instance's db file-backed (durable) at `dir`, replacing the
  // in-memory db the constructor opened. MMKV is a persistent store, so a
  // fair KV comparison needs kevy durable too: in-memory is fastest but
  // ephemeral, file-backed survives the process (AOF replay on open). Call
  // this once, right after creation, before any subscribe/data call. Returns
  // true if the durable store opened, false on failure (the instance is left
  // without a usable db — do not call getData/setData after a false). The
  // default (no call) stays in-memory, backward-compatible.
  openAt(dir: string): boolean

  // Poll-model pub/sub (bonus), mirroring kevy-ffi's polled sub. One
  // subscription per object; subNext drains one RESP frame or undefined.
  // publish returns the engine's receiver count (the `:N` PUBLISH reply —
  // Redis semantics), so callers and the TS local-fanout lane can report a
  // combined delivery count.
  subscribe(channel: string): void
  publish(channel: string, payload: ArrayBuffer): number
  subNext(): ArrayBuffer | undefined

  // Push-model pub/sub. A dedicated NATIVE poller thread blocks in
  // kevy_sub_wait_raw — a real kernel park on the engine's mpsc channel, so
  // it burns 0% CPU while idle (no busy-spin) — and, per frame, invokes this
  // JS callback, which Nitro auto-hops onto the JS thread (AsyncJSCallback ->
  // CallInvoker). That is JS-side push (one callback per message, zero
  // JS-side polling) with a native poller that sleeps until a frame arrives.
  // The push family drains the RESP-free lane: the callback receives the raw
  // message PAYLOAD (no `*3…message…` framing), and subscribe/unsubscribe
  // acks are skipped — a known-channel consumer wants only the bytes. (Need
  // the channel/kind? Use the framed subscribe/subNext lane above.)
  //
  // Per-message: one native->JS hop per frame.
  subscribePush(channel: string, onMessage: (frame: ArrayBuffer) => void): void
  // Batched: the poller drains ALL available frames per wake and delivers
  // them in ONE hop — amortizes the CallInvoker hop across a batch. To also
  // kill the M−1 JSI ArrayBuffer allocations a batch of M frames used to make
  // (one ArrayBuffer::move per frame), the poller memcpys each drained frame
  // as [u32-LE length prefix][bytes] into ONE growing buffer and delivers
  // ONE ArrayBuffer plus the frame count. The JS side slices Uint8Array views
  // (subarray, zero-copy) per frame by walking the u32 prefixes — see
  // unpackFrames() in index.ts.
  subscribePushBatched(
    channel: string,
    onBatch: (packed: ArrayBuffer, count: number) => void
  ): void
  // Stop the poller thread and close the push subscription.
  stopPush(): void
}

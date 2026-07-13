# kevy for Android / JVM

kevy embedded for **Kotlin and Java** — the real native engine in your
process, no server. One typed surface, `cmd()` to every verb, TTL,
structures, pub/sub, and persistence you can read (AOF + snapshots).
`libkevy_jni` (hand-written JNI over the kevy C ABI) + a Kotlin shell;
ships as an AAR with jniLibs for arm64-v8a / x86_64.

```kotlin
val db = KevyDB.open(context.filesDir.resolve("kevy").path)
// or KevyDB.openInMemory()

db.set("session:7f3a", payload, ttlMs = 3_600_000) // scalar fast path
db.getText("session:7f3a")

db.subscribe("room") { payload, channel ->
    Log.d("kevy", "$channel: ${String(payload)}")
}
db.publish("room", "hi")
db.poll() // drain pub/sub frames on your looper/timer

// The escape hatch: every verb, RESP semantics, errors as VALUES.
val reply = db.cmd("ZADD", "board", "42", "alice")

db.close()
```

Typed methods **throw** `KevyException` on a protocol error — a typed
call has one meaning. `cmd()` returns `KevyValue.Error` as a value
instead: driving the raw verb surface, the engine saying no is data.

`set`/`get` ride the C ABI's scalar fast path (no argv assembly, no
RESP framing) — the lane that answers an mmap KV's synchronous
read/write. Layout: `java/` is the bare JNI surface (`KevyNative`),
`kevy/` the Kotlin shell. Same API shape as every other kevy
embedding. Docs: <https://kevy.golia.jp>.

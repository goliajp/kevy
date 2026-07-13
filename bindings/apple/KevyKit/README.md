# KevyKit

kevy embedded for **Swift** (iOS 15+ / macOS 12+) — the real native
engine in your process, no server. One typed surface, `cmd()` to every
verb, TTL, structures, pub/sub, and persistence you can read (AOF +
snapshots). SwiftPM package wrapping `Kevy.xcframework` (ios-arm64 /
ios-sim-arm64 / macos-arm64 static libraries).

```swift
// Package.swift
.package(url: "https://github.com/goliajp/kevy", from: "4.0.0")
```

```swift
import KevyKit

let db = try KevyDB(dir: dataURL.path) // or KevyDB() for in-memory
try db.set("session:7f3a", payload, ttlMs: 3_600_000) // scalar fast path
let v = try db.getText("session:7f3a")

let sub = try db.subscribe("room")
_ = try db.publish("room", Data("hi".utf8))
while let frame = try sub.next() { /* poll on your cadence */ }

// The escape hatch: every verb, RESP semantics, errors as VALUES.
let reply = try db.cmd(["ZADD", "board", "42", "alice"])
```

Typed methods **throw** `KevyError` on a protocol error — a typed call
has one meaning. `cmd()` returns `.error(…)` as a value instead:
driving the raw verb surface, the engine saying no is data.

`set`/`get` ride the C ABI's scalar fast path (no argv assembly, no
RESP framing) — the lane that answers an mmap KV's synchronous
read/write. Same API shape as every other kevy embedding. Docs:
<https://kevy.golia.jp>.

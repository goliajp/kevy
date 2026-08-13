# KevyKit

kevy embedded for **Swift** (iOS 15+ / macOS 12+) — the real native
engine in your process, no server. One typed surface, `cmd()` to every
verb, TTL, structures, pub/sub, and persistence you can read (AOF +
snapshots). SwiftPM package wrapping `Kevy.xcframework` (ios-arm64 /
ios-sim-arm64 / macos-arm64 static libraries).

```swift
// Package.swift
.package(url: "https://github.com/goliajp/kevy", from: "5.1.0")
```

The package manifest lives at the **repository root** — SwiftPM resolves
a package from the root of the repository it clones, so that is the only
place it can be reached from. Sources, tests and `Artifacts/` stay here.

```swift
```

> **Pre-release.** `v5.1.0` is tagged, but the repo root carries no
> `Package.swift` — SwiftPM cannot resolve the URL form above yet.
> Until the package manifest is hoisted, depend on the package by path:
> `.package(path: "/path/to/kevy/bindings/apple/KevyKit")`, after
> `bash packaging/apple/build-xcframework.sh` has produced
> `Artifacts/Kevy.xcframework`.

```swift
import KevyKit

let db = try KevyDB(dir: dataURL.path) // or KevyDB() for in-memory
try db.set("session:7f3a", payload, ttlMs: 3_600_000) // scalar set lane
let v = try db.getText("session:7f3a")

let sub = try db.subscribe("room")
_ = try db.publish("room", Data("hi".utf8))
while let frame = try sub.next() { /* poll on your cadence */ }
if let frame = try sub.wait(timeoutMs: 1000) { /* or park in the kernel */ }

// The escape hatch: every verb, RESP semantics, errors as VALUES.
let reply = try db.cmd(["ZADD", "board", "42", "alice"])
```

Typed methods **throw** `KevyError` on a protocol error — a typed call
has one meaning. A recognized store-semantic reply (a WRONGTYPE `get`
on a non-string key) throws the structured `.store(.wrongType)`, the
same variant the siblings raise. `cmd()` returns `.error(…)` as a value
instead: driving the raw verb surface, the engine saying no is data.

`set` rides the C ABI's scalar `kevy_set` lane (no argv assembly, no
RESP framing) and passes the value straight from its `Data` storage.
`get` rides the zero-copy shared lane (`kevy_get_shared`): a bulk value
is an engine `Arc` clone (refcount bump, no byte copy) that the returned
`Data` views without copying — the mmap-view analog — pinning the Arc
until the `Data` is freed. This drops the engine-side and client-side
copies on large values, but that delta is **unmeasured on Swift** and
may be throughput-neutral; it is adopted as the correct lane + parity
with the siblings, not on a benched speedup. Same API shape as every
other kevy embedding. Docs: <https://kevy.golia.jp>.

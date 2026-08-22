# Kevy (.NET)

The first-party **kevy** client for .NET — one `Connect(url)` to either the
in-process embedded engine (`mem://` / `file://`) or a remote RESP server
(`kevy://` / `redis://` / `tcp://`), every command family on **both** a
synchronous and an async `Task` face. Same package also ships the raw
embedded door (`Kevy.Embedded.KevyDb`, below). Pure Rust engine, no server.

This document tracks kevy **5.4.0**.

```bash
dotnet add package kevy
```

The package is managed code only. `mem://` and `file://` additionally
need `libkevy_ffi`, a per-platform native library — see
[The embedded engine](#the-embedded-engine) below; a remote URL needs
nothing else.

```csharp
using Kevy;

using var c = KevyClient.Connect("kevy://127.0.0.1:6379");   // or "mem://app"
c.Set("k", "v");
c.Get("k");                                       // byte[]  ("v")
await c.SetAsync("k", "v2");                       // async twin, same client
await c.GetAsync("k");

c.HSet("h", "f", "1");
c.ZAdd("board", new ZMember(42, "alice"u8.ToArray()));

// Pub/sub: publish from a Client, consume from a Subscriber on the same URL.
using var sub = Subscriber.ConnectChannels("mem://app", "room");
c.Publish("room", "hi");
var (channel, payload) = sub.RecvMessage();

// The mandatory raw escape hatch — every verb; a -ERR is Reply.Error data.
Reply r = c.Do("IDX.QUERY", "byage", "RANGE", "0", "100");
```

Errors are a `KevyException` hierarchy: a recognized store error surfaces as
`KevyStoreException` (`StoreError.WrongType`, …) on both backends; any other
`-ERR` is a `KevyProtocolException` with the wire text preserved. Remote-only
families — `IDX.*`, `MULTI`/`EXEC`, pipeline, cluster — throw
`KevyUnsupportedException` on the embedded backend (reach for `Do(...)`).

## The embedded engine

`mem://`, `file://` and everything under `Kevy.Embedded` run kevy inside
your process through `libkevy_ffi`, a per-platform native library. The
NuGet package does not carry it: it would work on the handful of
runtime identifiers we happened to build and nowhere else, while the
managed remote client works wherever .NET does. So the engine comes
from the engine's own repository —

```bash
git clone https://github.com/goliajp/kevy
cd kevy && cargo build --release -p kevy-ffi   # → target/release/
```

— and is loaded by pointing `KEVY_FFI_LIB` at the built library, or by
placing it where the runtime probes (`runtimes/<rid>/native/` in a
published app). Without it, the first embedded call throws a
`KevyException` saying exactly this, with the underlying
`DllNotFoundException` as its `InnerException`.

```csharp
using Kevy.Embedded;

using var db = KevyDb.Open("data/"); // or KevyDb.OpenInMemory()
db.Set("session:7f3a", "payload", ttlMs: 3_600_000);
db.GetText("session:7f3a");          // "payload"

db.Subscribe("room", (payload, channel) =>
    Console.WriteLine($"{channel}: {Encoding.UTF8.GetString(payload)}"));
db.Publish("room", "hi");
db.Poll(); // drain pub/sub frames on your loop/timer

// The escape hatch: every verb, RESP semantics, errors as VALUES.
var reply = db.Cmd("ZADD", "board", "42", "alice");
```

Typed methods (`Set` / `Get` / `GetText` / `Del` / `IncrBy` / `Expire`
/ `PttlMs` / `Keys` / `Mget` / `DbSize` / `FlushAll` / `Publish` /
`Subscribe`) **throw** `KevyException` on a protocol error — a typed
call has one meaning. `Cmd()` returns `KevyValue.Error` as a value
instead: driving the raw verb surface, the engine saying no is data.

Same API shape as every other kevy embedding (Node/Bun, Go, Swift,
Kotlin, wasm). Docs: <https://kevy.golia.jp>.

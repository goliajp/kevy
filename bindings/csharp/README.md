# Kevy (.NET)

kevy embedded in **.NET** — the real native engine in your process, no
server. One typed surface, `Cmd()` to every verb, TTL, structures,
pub/sub, and persistence you can read (AOF + snapshots). net8
`LibraryImport` P/Invoke over a prebuilt cdylib shipped under
`runtimes/<rid>/native/` — nothing compiles on install.

```bash
dotnet add package Kevy
```

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

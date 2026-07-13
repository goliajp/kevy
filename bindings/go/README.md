# kevy-go

kevy embedded in **Go** — the real native engine in your process, no
server. One typed surface, `Cmd()` to every verb, TTL, structures,
pub/sub, and persistence you can read (AOF + snapshots). cgo over a
static library vendored per target (the wasmtime-go model) — no
system dependency to install.

```bash
go get github.com/goliajp/kevy-go
```

```go
import kevy "github.com/goliajp/kevy-go"

db, err := kevy.Open("data/") // or kevy.OpenMem()
if err != nil { panic(err) }
defer db.Close()

db.SetEx("session:7f3a", []byte("payload"), time.Hour)
v, ok, _ := db.Get("session:7f3a") // []byte("payload"), true

sub, _ := db.Subscribe("room")
db.Publish("room", []byte("hi"))
frame, ok, _ := sub.Next() // poll; RESP frame as kevy.Value

// The escape hatch: every verb, RESP semantics, errors as VALUES.
reply, _ := db.Cmd("ZADD", "board", "42", "alice")
```

Typed methods (`Set` / `SetEx` / `Get` / `Del` / `Incr` / `IncrBy` /
`Expire` / `PTTL` / `Keys` / `MGet` / `DBSize` / `FlushAll` /
`Publish` / `TypeOf`) return an `error` on a protocol error — a typed
call has one meaning. `Cmd()` returns the error **as a `Value`**
instead: driving the raw verb surface, the engine saying no is data.

Same API shape as every other kevy embedding (Node/Bun, .NET, Swift,
Kotlin, wasm). Docs: <https://kevy.golia.jp>.

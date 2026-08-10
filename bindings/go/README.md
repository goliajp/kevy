# kevy-go

The first-party **Go** client for kevy — the pure-Rust Redis-compatible
engine. One import ships both faces of the [client contract](../../docs/client-contract.md):

- **Embedded** (`mem://` / `file://`): the real native engine in your
  process, no server. cgo over a static library (`crates/kevy-ffi`) — no
  system dependency to install.
- **Remote** (`kevy://` / `redis://` / `tcp://`): a native RESP2/RESP3 TCP
  client. Same business code, switch backends by changing only the URL.

> **Pre-release.** This client tracks kevy **5.0.0**. The standalone module repo does not exist yet, so
> `go get github.com/goliajp/kevy-go` is not runnable. Until then, use this
> client from the in-repo copy at `bindings/go` (clone the kevy repo and
> import the local module, or `replace github.com/goliajp/kevy-go => ./bindings/go`
> in your own `go.mod`). The path below is the intended post-release form.

```bash
go get github.com/goliajp/kevy-go
```

```go
import (
	"context"
	kevy "github.com/goliajp/kevy-go"
)

ctx := context.Background()

// Embedded in-process, or "kevy://127.0.0.1:6379" for a server — same code.
c, err := kevy.Connect("mem://app")
if err != nil { panic(err) }
defer c.Close()

c.Set(ctx, []byte("k"), []byte("v"))
v, ok, _ := c.Get(ctx, []byte("k"))       // []byte("v"), true
c.ZAdd(ctx, []byte("board"), kevy.ZMember{Score: 42, Member: []byte("alice")})

// Errors are values: a typed KevyError, inspectable by variant.
if _, err := c.Incr(ctx, []byte("k")); err != nil {
	if se, ok := kevy.StoreErrorOf(err); ok && se == kevy.StoreNotInteger {
		// structured store error
	}
}

// Raw escape hatch: every verb reachable, RESP reply as data.
reply, _ := c.Do(ctx, []byte("COMMAND"), []byte("COUNT"))
```

## Both faces, one client

Blocking methods take a `context.Context` for cancellation/timeout. The
async face is reached via `c.Async()`, returning a `*Async` whose methods
resolve a `Future[T]` — they delegate to the same blocking methods, so
sync and async always agree:

```go
a := c.Async()
f := a.Get(ctx, []byte("k"))
res, _ := f.Await(ctx)                       // {Value, OK}

// Generic async for any op:
n, _ := kevy.GoAsync(a, ctx, func(ctx context.Context, c *kevy.Client) (int64, error) {
	return c.DBSize(ctx)
}).Await(ctx)
```

## Coverage

Core KV, hash, list, set, zset, zset-algebra, hash-field TTL, blocking
pops, `IDX.*` (typed + raw), `VIEW.*`/`FEED.*` (raw + typed feed),
pub/sub (`Subscriber`), transactions (`MULTI`/`EXEC`/`WATCH`), pipeline,
and a CRC16-routed `ClusterClient`. The embedded store (`DB`) exposes the
`Cmd` / scalar `GetScalar`/`SetScalar` / `Subscribe` surface directly.

Docs: <https://kevy.golia.jp>.

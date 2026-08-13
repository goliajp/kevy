# kevy-go

The first-party **Go** client for kevy — the pure-Rust Redis-compatible
engine. One import ships both faces of the [client contract](../../docs/client-contract.md):

- **Remote** (`kevy://` / `redis://` / `tcp://`): a native RESP2/RESP3 TCP
  client. Pure Go, standard library only — this is what `go get` gives you.
- **Embedded** (`mem://` / `file://`): the real native engine in your
  process, no server. cgo over the static library built from
  `crates/kevy-ffi`, so it is reached from this tree behind the
  `kevy_embedded` build tag (see below). Same business code either way:
  switch backends by changing only the URL.

This client tracks kevy **5.1.0**.

```bash
go get github.com/goliajp/kevy-go/v5
```

That module is generated from this directory and pushed on each release
— Go has no registry, so an import path has to be a repository URL.
Edit here, never there. See [PUBLISH-FORM.md](PUBLISH-FORM.md).

```go
import (
	"context"
	kevy "github.com/goliajp/kevy-go/v5"
)

ctx := context.Background()

// A server here; "mem://app" runs the engine in-process instead — same
// code, and see the embedded section below for what that one needs.
c, err := kevy.Connect("kevy://127.0.0.1:6379")
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

## The embedded engine

`mem://` and `file://` run the engine inside your process, over its C
ABI. That backend is cgo linked against a per-platform static library,
which is far too large to travel through the Go module proxy — a Go
module ships source — so it is not in the published module. It is built
from the kevy tree, where cargo has just produced the library:

```bash
cargo build -p kevy-ffi   # debug: that is the path the cgo preamble links
cd bindings/go && go test -tags kevy_embedded ./...
```

Without the tag, `mem://` and `file://` return an error saying exactly
this. Nothing else in the API changes.

## Coverage

Core KV, hash, list, set, zset, zset-algebra, hash-field TTL, blocking
pops, `IDX.*` (typed + raw), `VIEW.*`/`FEED.*` (raw + typed feed),
pub/sub (`Subscriber`), transactions (`MULTI`/`EXEC`/`WATCH`), pipeline,
and a CRC16-routed `ClusterClient`. The embedded store (`DB`) exposes the
`Cmd` / scalar `GetScalar`/`SetScalar` / `Subscribe` surface directly.

Docs: <https://kevy.golia.jp>.

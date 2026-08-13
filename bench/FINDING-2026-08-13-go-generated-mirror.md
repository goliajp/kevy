# Go: publishing a language that has no registry

What it took to put the Go client on its channel, and the three things
that were wrong on the way. Recorded because two of them are shaped
like defects we have paid for before in other channels, and the third
was a coverage regression I introduced myself while fixing the first.

## The constraint

Go has no package registry. An import path *is* a repository URL, so
`import "github.com/goliajp/kevy-go/v5"` requires a repository at that
exact address with `go.mod` at its root. Every other binding publishes
by uploading an artifact; this one publishes by having a second
repository exist.

The owner's instruction was that kevy-go must never be maintained
independently — all operations happen from kevy. So it is generated:
`scripts/mirror-go-module.sh` produces it, verifies it, pushes it, and
tags it. Nothing else writes it.

## Generating by omission

A generator has to know what to leave out. The embedded engine is cgo
against `crates/kevy-ffi`, with a preamble pointing two directories up:

```go
#cgo CFLAGS: -I${SRCDIR}/../../crates/kevy-ffi/include
#cgo LDFLAGS: ${SRCDIR}/../../target/debug/libkevy_ffi.a
```

Those paths leave the module. They work in this tree and break the
instant it is extracted.

The published module is therefore the pure-Go half, selected by build
tag. That is not a concession: a Go module ships *source* through the
proxy, so a per-platform static library of tens of megabytes could not
travel with it under any design, and pure Go works on every platform Go
supports rather than the three we would have vendored. `go get` gives a
working RESP client with no cgo and no toolchain.

The seam is `embedded_seam.go`. The tagged files register themselves
into two function variables at init; everything else reaches the engine
through interfaces. The alternative — a parallel set of stub files —
is a second definition of the same surface, and it drifts the first
time someone adds a method to one of them.

Writing those interfaces by hand got five signatures wrong on the first
attempt (`CmdBytes`'s return, `SetScalar`'s ttl argument, `subBytes`'s
arity, `Close`'s error, `Sub.Next`'s triple return). The compiler named
every one. Worth stating plainly because the temptation was to write
them from memory of the calling code rather than from the methods.

## Three defects

### 1. The gate that only works where it was extracted from

First version of the generator staged the mirror in a temp directory
under the repo and tested it there. It passed. It would have passed
even if the cgo paths had been left in, because from *under* the kevy
tree `${SRCDIR}/../../crates` still resolves.

A module that only works where it was extracted from is precisely the
failure the mirror exists to prevent, and the check as written could
not see it. The generator now builds and tests outside the tree.

Same family: the mirror's remote tests need a server to talk to, and
without one they skip — leaving a fully skipped suite, which is also
green. A missing binary is a failure there, not a skip, and the
generator additionally refuses a run in which nothing passed.

### 2. A tag that silently emptied a job

Putting the embedded files behind `kevy_embedded` changed what
`go test ./...` means. Two places ran exactly that:

- the **ffigate** CI job, which builds `libkevy_ffi` and then exercises
  each language door against it — its Go step became a pure-Go run,
  testing none of the door it had just built the library for;
- the **embeddedgate** bench harness, whose entire subject is that
  backend.

Both went green while measuring nothing. Adding a build tag is a
question about every existing invocation, not only the new one.

While there: `serverBinary` held an absolute path to one developer's
machine among its candidates, so the tests passed there for a reason
the code did not state. It reads `KEVY_SERVER_BIN` now — which is also
how the generated module, with no kevy tree above it, finds a server.

### 3. Two pushes are a window

The push step was a branch push followed by a tag push. Between them
the repository has the commit and not the tag, and anything fetching
during that window records `unknown revision` — and caches it for far
longer than the window lasted.

That is what happened to v5.1.0. The symptom was contradictory:

| endpoint | answer |
|---|---|
| `@latest` | 200, full metadata, correct commit hash and ref |
| `@v/list` | 200, `v5.1.0` |
| `@v/v5.1.0.info` / `.mod` / `.zip` | 404 |
| `sum.golang.org/lookup/…` | 404 `unknown revision v5.1.0` |

`GOPROXY=direct GOSUMDB=off go get` worked throughout, which is what
separated "the artifact is broken" from "a cache is stale". It also
showed the two services are in **series**, not parallel: the checksum
database hashes the zip it gets from the proxy, so while the proxy
404s the sumdb cannot record anything, and `go get` reports the proxy's
failure wearing the sumdb's URL.

It cleared on its own after roughly half an hour. The push is one
`--atomic` ref update now, so the window does not exist.

## Where it is enforced

- `go-mirror` CI job — generates, builds and tests outside the tree,
  then diffs against the published module. Both branches of that diff
  have been observed: it reported drift before the push and in-sync
  after.
- `bash scripts/mirror-go-module.sh --push X.Y.Z` — the release step,
  documented in the release skill. It refuses a version already tagged
  (a Go version is immutable once the proxy has fetched it, so moving a
  tag changes nothing for anyone who already resolved it) and refuses a
  version whose major disagrees with the module path's `/vN`.

## Verified

`go get github.com/goliajp/kevy-go/v5@v5.1.0` through the default
proxy, in a fresh module, on a machine with no kevy checkout: resolves,
records checksums, compiles, runs. `mem://` returns the sentence naming
the build tag and where to build it.

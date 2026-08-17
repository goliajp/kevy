# Go module publish form — resolved

How `github.com/goliajp/kevy-go` is produced, and why it does not carry
the embedded engine. Recorded because the earlier plan in this file said
the opposite, and someone reading the code later deserves to know the
plan changed on purpose rather than by neglect.

## The shape

`kevy-go` is **generated** from this directory by
`scripts/mirror-go-module.sh` and pushed on each release. It contains
the pure-Go files only: everything without the `kevy_embedded` build
tag. No cgo, no vendored natives, no dependencies beyond the standard
library.

```
kevy-go/
  go.mod            module github.com/goliajp/kevy-go/v5
  *.go              32 files, copied verbatim
  README.md         generated; says it is generated
  LICENSE-APACHE, LICENSE-MIT
```

Nobody edits it. Changes are made here; the release regenerates it.

## Why it exists at all

Go has no package registry. An import path *is* a repository URL, so
`import "github.com/goliajp/kevy-go/v5"` requires a repository at that
exact address with `go.mod` at its root. Every other binding publishes
by uploading a built artifact to a registry; this one publishes by
having a second repository exist. `bindings/go` inside this tree cannot
be imported by anyone, no matter what is in it.

## Why the embedded engine is not in it

The earlier plan was to vendor `libkevy_ffi.a` for three targets, the
way the apple door carries an xcframework and the android door carries
jniLibs. That plan does not survive contact with how Go distributes
code:

- **A Go module ships source.** Consumers fetch through
  `proxy.golang.org`, which serves a zip of the repository. Static
  libraries for three targets are tens of megabytes each, and every
  version ever published stays in the proxy's cache forever.
- **It buys nothing for most users.** The remote RESP client is what a
  Go service normally wants; the embedded engine is for a process that
  wants the store *inside* it, and that process is being built from a
  tree where cargo has just produced the library anyway.
- **It would cap the platforms.** Vendoring three `.a` files makes the
  module work on three platforms. Pure Go works everywhere Go works,
  including `GOOS=windows` and `js/wasm`, neither of which the engine
  builds for today.

So the split is by build tag. Without `kevy_embedded`, `mem://` and
`file://` return an error naming the fix. With it — inside this tree,
where the relative cgo paths resolve — the engine is linked in.

```sh
cargo build -p kevy-ffi   # debug: that is the path the cgo preamble links
go test -tags kevy_embedded ./...
```

The seam is `embedded_seam.go`: the tagged files register themselves
into two function variables at init, and everything else talks to the
engine through interfaces. That is what lets the mirror be produced by
*omitting* files rather than by maintaining a parallel set of stubs — a
stub set would be a second definition of the same surface, and the two
would drift the first time someone added a method to one.

## The `/vN` suffix

`go.mod` declares `github.com/goliajp/kevy-go/v5`, and the tag is
`v5.3.0`. Go requires the major version in the path for v2 and up; the
alternative — starting kevy-go's own versioning at v1, independent of
the engine — was rejected because a user comparing `kevy-go v1.2.0`
against a `kevy 5.3.0` server has no way to tell whether they match.
Tracking the engine's version makes the question answerable by reading.

The mirror script refuses to push if the path suffix and the version's
major disagree, because that mismatch is not a build error — it is a
module that resolves to the wrong major forever.

## Guarantees, and where they are checked

The `go-mirror` CI job runs the generator on every push. It builds the
module **outside** the kevy tree — under it, the cgo paths would still
resolve, and a module that only works where it was extracted from would
pass the check and fail for its first real user — then vets it and runs
its tests against a real server binary. A missing binary is a failure
there, not a skip: a fully skipped suite is also green.

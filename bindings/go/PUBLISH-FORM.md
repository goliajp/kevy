# Go module publish form — audit

What `go get github.com/goliajp/kevy-go` must resolve to, versus what the
in-repo `bindings/go` is today. This is a checklist for the t6 channel
release, not something to build now: embedding per-target static libraries
is part of publishing, and the standalone repo does not exist yet.

## The gap

The embedded engine is cgo over `crates/kevy-ffi`. The current preamble
(`kevy.go`) points at the kevy checkout by relative path:

```go
#cgo CFLAGS: -I${SRCDIR}/../../crates/kevy-ffi/include
#cgo LDFLAGS: ${SRCDIR}/../../target/debug/libkevy_ffi.a
```

Both `../..` paths leave the module. They work here because `bindings/go`
sits inside the kevy tree; they break the moment the module is a
standalone repo, and `target/debug` is a dev build regardless. So a
published `go get` would compile the Go and then fail to link.

## The publish form

A standalone module carries its own natives, the way apple carries an
xcframework and android carries jniLibs:

```
kevy-go/
  go.mod                       module github.com/goliajp/kevy-go
  *.go
  include/kevy.h               vendored from crates/kevy-ffi/include
  libs/
    darwin_arm64/libkevy_ffi.a
    linux_amd64/libkevy_ffi.a
    linux_arm64/libkevy_ffi.a
```

with the preamble resolving to the vendored copies, per-target:

```go
#cgo CFLAGS: -I${SRCDIR}/include
#cgo darwin,arm64 LDFLAGS: ${SRCDIR}/libs/darwin_arm64/libkevy_ffi.a
#cgo linux,amd64  LDFLAGS: ${SRCDIR}/libs/linux_amd64/libkevy_ffi.a
#cgo linux,arm64  LDFLAGS: ${SRCDIR}/libs/linux_arm64/libkevy_ffi.a
#cgo linux LDFLAGS: -lpthread -ldl -lm
```

`GOFLAGS=-mod=mod` and cgo on; nothing about the Go source changes, only
where the `.a` and the header come from.

## Checklist for the release

- [ ] Standalone repo `github.com/goliajp/kevy-go` created.
- [ ] `include/kevy.h` vendored (not a `../..` reference).
- [ ] `libs/<goos>_<goarch>/libkevy_ffi.a` built release, three targets to
      match the other doors (darwin-arm64, linux-amd64, linux-arm64).
- [ ] Preamble switched to `${SRCDIR}/include` + per-target `libs/…`.
- [ ] `go.mod` tagged `v5.0.0`, so `go get …@v5.0.0` resolves — the module
      path already declares no `/v4` suffix issue because the tag is the
      first v4 (a v2+ module normally needs a `/vN` path suffix; confirm
      whether kevy-go adopts `/v4` or ships as its own first major).
- [ ] `pkg.go.dev` front-door README (the README already written here,
      copied or referenced from the standalone repo).

## Note on the /vN suffix

Go modules require the major version in the path for v2 and up:
`github.com/goliajp/kevy-go/v4`. If kevy-go's own history starts at v4 to
track the engine, that suffix is mandatory and the current
`module github.com/goliajp/kevy-go` line is wrong for a v4 tag. This is the
one decision the release must make consciously — either start kevy-go's
versioning independently (its own v1) or adopt the `/v4` path to mirror the
engine. Left to the channel-release owner.

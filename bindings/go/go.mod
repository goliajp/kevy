// Home: github.com/goliajp/kevy-go. Go has no registry — an import path
// IS a repository URL — so this name requires that repository to exist.
// It does, and it is GENERATED from this directory by
// scripts/mirror-go-module.sh; this copy is the source of truth and the
// ffigate target. See PUBLISH-FORM.md.
//
// The /v5 suffix is not optional. Go's semantic import versioning
// requires a /vN element in the module path for every major version at
// or above 2, so a module declared without it cannot be tagged v5.1.0
// at all: `go get` rejects the mismatch rather than resolving it. The
// path and the release tag move together from here on.
module github.com/goliajp/kevy-go/v5

go 1.22

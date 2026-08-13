// Home: github.com/goliajp/kevy-go. Go has no registry — an import path
// IS a repository URL — so this name requires that repository to exist;
// this in-repo copy stays the source of truth and the ffigate target.
//
// The /v5 suffix is not optional. Go's semantic import versioning
// requires a /vN element in the module path for every major version at
// or above 2, so a module declared without it cannot be tagged v5.1.0
// at all: `go get` rejects the mismatch rather than resolving it. The
// path and the release tag move together from here on.
//
// If a separate repository is not wanted, the no-new-repo form is a
// subdirectory module in this one — `module
// github.com/goliajp/kevy/bindings/go/v5`, released with tags shaped
// `bindings/go/v5.1.0`. It costs a longer import path and leaks our
// internal layout into a public API, which is why the standalone name
// is preferred.
module github.com/goliajp/kevy-go/v5

go 1.22

// embeddedgate Go harness — kevy-go scalar vs bbolt / badger.
// See bench/EMBEDDED-LEDGER.md.
// kevy-go is linked via replace to the in-repo source of truth; its cgo
// preamble links target/debug/libkevy_ffi.a, so run.sh stages the RELEASE
// static lib there for a fair perf comparison and restores it after.
module kevy-embeddedgate-go

go 1.25.0

require (
	github.com/dgraph-io/badger/v4 v4.9.4
	github.com/goliajp/kevy-go/v6 v5.0.0
	go.etcd.io/bbolt v1.5.0
)

require (
	github.com/cespare/xxhash/v2 v2.3.0 // indirect
	github.com/dgraph-io/ristretto/v2 v2.2.0 // indirect
	github.com/dustin/go-humanize v1.0.1 // indirect
	github.com/go-logr/logr v1.4.3 // indirect
	github.com/go-logr/stdr v1.2.2 // indirect
	github.com/google/flatbuffers v25.2.10+incompatible // indirect
	github.com/klauspost/compress v1.18.0 // indirect
	go.opentelemetry.io/auto/sdk v1.1.0 // indirect
	go.opentelemetry.io/otel v1.37.0 // indirect
	go.opentelemetry.io/otel/metric v1.37.0 // indirect
	go.opentelemetry.io/otel/trace v1.37.0 // indirect
	golang.org/x/sys v0.45.0 // indirect
	google.golang.org/protobuf v1.36.7 // indirect
)

replace github.com/goliajp/kevy-go/v6 => ../../../bindings/go

// Cross-language counterpart of kevy-hash's bench, Go side:
// hash/maphash (Go's runtime hashtable hash, akin to AES-NI on
// amd64/arm64) on the same hash_bytes / hash_u64 workloads.

package main

import (
	"encoding/binary"
	"fmt"
	"hash/maphash"
	"runtime"
	"sort"
	"time"
)

const (
	iter    = 1_000_000
	samples = 25
	host    = "M4-Pro-aarch64"
	stone   = "kevy-hash"
	lang    = "go"
)

var gSink uint64

//go:noinline
func sinkU64(x uint64) { gSink ^= x }

func timeOne(f func() uint64) uint64 {
	runtime.GC()
	t0 := time.Now()
	var acc uint64
	for i := 0; i < iter; i++ {
		acc ^= f()
	}
	d := time.Since(t0)
	sinkU64(acc)
	return uint64(d.Nanoseconds() / int64(iter))
}

func bench(competitor, workload string, f func() uint64) {
	times := make([]uint64, 0, samples)
	for i := 0; i < samples; i++ {
		times = append(times, timeOne(f))
	}
	sort.Slice(times, func(i, j int) bool { return times[i] < times[j] })
	med := times[samples/2]
	p95 := times[(samples*95)/100]
	min := times[0]
	now := time.Now().UTC().Format("2006-01-02T15:04:05Z")
	fmt.Printf(
		"{\"stone\":\"%s\",\"language\":\"%s\",\"competitor\":\"%s\","+
			"\"workload\":\"%s\",\"metric\":\"ns_per_op\","+
			"\"value_median\":%d,\"value_p95\":%d,\"value_min\":%d,"+
			"\"iterations\":%d,\"host\":\"%s\",\"date\":\"%s\"}\n",
		stone, lang, competitor, workload,
		med, p95, min, iter, host, now,
	)
}

func main() {
	var seed = maphash.MakeSeed()

	b8 := []byte{0, 1, 2, 3, 4, 5, 6, 7}
	b16 := make([]byte, 16)
	for i := range b16 {
		b16[i] = byte(i)
	}
	b64 := make([]byte, 64)
	for i := range b64 {
		b64[i] = byte(i)
	}

	// maphash.Bytes is the one-call form Go added in 1.19; the most
	// direct comparator to kevy-hash's KevyHash trait.
	bench("hash/maphash.Bytes", "hash_bytes_8B", func() uint64 {
		return maphash.Bytes(seed, b8)
	})
	bench("hash/maphash.Bytes", "hash_bytes_16B", func() uint64 {
		return maphash.Bytes(seed, b16)
	})
	bench("hash/maphash.Bytes", "hash_bytes_64B", func() uint64 {
		return maphash.Bytes(seed, b64)
	})

	// u64: pack a u64 to 8 bytes then hash. This matches what a Go
	// hashtable does for a uint64 key (the runtime has a direct
	// memhash_u64 path; this is the publicly-exposed equivalent).
	n := uint64(0xdeadbeefcafebabe)
	var u64buf [8]byte
	binary.LittleEndian.PutUint64(u64buf[:], n)
	bench("hash/maphash.Bytes", "hash_u64", func() uint64 {
		binary.LittleEndian.PutUint64(u64buf[:], n)
		return maphash.Bytes(seed, u64buf[:])
	})
}

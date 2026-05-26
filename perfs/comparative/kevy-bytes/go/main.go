// Cross-language counterpart of kevy-bytes' bench, Go side: measures
// the standard `string` (shared/immutable, 16-byte header) and `[]byte`
// (owned, 24-byte slice header) on the same clone / eq / from_*
// workloads. Emits one JSON line per measurement (schema in
// perfs/comparative/README.md).
//
// Build: `go build -o bench ./...`
// Run:   `./bench`
//
// Notes:
//   - Each workload runs an inner loop of `iter` iterations, timed as
//     a whole, divided to ns/op. SAMPLES repeats produce med/p95/min.
//   - The escape-analysis sinks `sinkString` / `sinkBytes` / `sinkBool`
//     defeat dead-store elimination so the compiler can't strip the
//     loop body.

package main

import (
	"bytes"
	"fmt"
	"runtime"
	"sort"
	"time"
)

const (
	iter    = 1_000_000
	samples = 25
	host    = "M4-Pro-aarch64"
	stone   = "kevy-bytes"
	lang    = "go"
)

//go:noinline
func sinkString(s string) {
	if len(s) > 1<<30 {
		fmt.Println(s[0])
	}
}

//go:noinline
func sinkBytes(b []byte) {
	if len(b) > 1<<30 {
		fmt.Println(b[0])
	}
}

//go:noinline
func sinkBool(b bool) {
	if b && time.Now().UnixNano() < 0 {
		fmt.Println("never")
	}
}

func timeOne(iter int, f func()) uint64 {
	runtime.GC()
	t0 := time.Now()
	for i := 0; i < iter; i++ {
		f()
	}
	d := time.Since(t0)
	return uint64(d.Nanoseconds() / int64(iter))
}

func bench(competitor, workload string, f func()) {
	times := make([]uint64, 0, samples)
	for i := 0; i < samples; i++ {
		times = append(times, timeOne(iter, f))
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
	shortStr := "hello world!"           // 12 bytes
	shortBytes := []byte(shortStr)
	longBytes := make([]byte, 64)
	for i := range longBytes {
		longBytes[i] = byte(i)
	}
	longStr := string(longBytes)

	// ---- string clone (shared semantic — interface compatible to kevy
	//      Bytes but NOT owned-copy; reported for completeness, NOT in
	//      the owned-cohort gate). ----
	{
		src := shortStr
		bench("string (shared)", "clone_inline_12B", func() {
			c := src
			sinkString(c)
		})
	}
	{
		src := longStr
		bench("string (shared)", "clone_heap_64B", func() {
			c := src
			sinkString(c)
		})
	}

	// ---- []byte clone (owned-copy semantic — comparable to kevy-bytes). ----
	{
		src := shortBytes
		bench("[]byte (owned)", "clone_inline_12B", func() {
			c := make([]byte, len(src))
			copy(c, src)
			sinkBytes(c)
		})
	}
	{
		src := longBytes
		bench("[]byte (owned)", "clone_heap_64B", func() {
			c := make([]byte, len(src))
			copy(c, src)
			sinkBytes(c)
		})
	}

	// ---- eq (string) ----
	{
		a, b := shortStr, shortStr
		bench("string (shared)", "eq_inline_12B", func() {
			sinkBool(a == b)
		})
	}
	{
		a, b := longStr, longStr
		bench("string (shared)", "eq_heap_64B", func() {
			sinkBool(a == b)
		})
	}

	// ---- eq ([]byte) ----
	{
		a := append([]byte(nil), shortBytes...)
		b := append([]byte(nil), shortBytes...)
		bench("[]byte (owned)", "eq_inline_12B", func() {
			sinkBool(bytes.Equal(a, b))
		})
	}
	{
		a := append([]byte(nil), longBytes...)
		b := append([]byte(nil), longBytes...)
		bench("[]byte (owned)", "eq_heap_64B", func() {
			sinkBool(bytes.Equal(a, b))
		})
	}

	// ---- from_bytes ([]byte from another []byte = clone) ----
	bench("[]byte (owned)", "from_bytes_inline_12B", func() {
		c := append([]byte(nil), shortBytes...)
		sinkBytes(c)
	})
	bench("[]byte (owned)", "from_bytes_heap_64B", func() {
		c := append([]byte(nil), longBytes...)
		sinkBytes(c)
	})

	// ---- from_str (string from []byte = single copy) ----
	bench("string (shared)", "from_str_inline_12B", func() {
		s := string(shortBytes)
		sinkString(s)
	})
	bench("string (shared)", "from_str_heap_64B", func() {
		s := string(longBytes)
		sinkString(s)
	})
}


// Package kevy is the first-party Go client for kevy — the pure-Rust
// Redis-compatible engine. It ships two things behind one import:
//
//   - The embedded engine (this file + emb_sub.go), bound to the C ABI in
//     crates/kevy-ffi: an in-process Store reachable through one Cmd path
//     plus scalar Get/Set and polled pub/sub (contract §5).
//   - The unified Client (client*.go), which routes a single Connect(url)
//     to either the embedded engine (mem:// / file://) or a native RESP
//     TCP server (kevy:// / redis:// / tcp://), exposing every command
//     family with both a blocking and an async face (contract §1–§4).
//
// A protocol error (-ERR …) from the embedded Cmd path is a Reply with
// Kind == KindError, not a Go error: the engine answering "no" is a
// working engine. The typed Client methods, by contrast, map -ERR to a
// *KevyError, because a typed call has exactly one meaning.
package kevy

/*
#cgo CFLAGS: -I${SRCDIR}/../../crates/kevy-ffi/include
#cgo LDFLAGS: ${SRCDIR}/../../target/debug/libkevy_ffi.a
#cgo linux LDFLAGS: -lpthread -ldl -lm
#include <stdlib.h>
#include "kevy.h"
*/
import "C"

import (
	"errors"
	"runtime"
	"unsafe"
)

// DB is an open embedded engine — the kevy-embedded surface (contract
// §5.2). Close it exactly once. Every method is safe from multiple
// goroutines on the same handle (the C ABI serialises internally).
type DB struct {
	p *C.KevyDb
}

// Open opens a persistent store rooted at dir, replaying its log on open
// (snapshot then AOF). Flushes and closes durably.
func Open(dir string) (*DB, error) {
	b := []byte(dir)
	var ptr *C.uint8_t
	if len(b) > 0 {
		ptr = (*C.uint8_t)(unsafe.Pointer(&b[0]))
	}
	p := C.kevy_open(ptr, C.size_t(len(b)))
	runtime.KeepAlive(b)
	if p == nil {
		return nil, errors.New("kevy: open failed")
	}
	return &DB{p: p}, nil
}

// OpenMem opens a pure in-memory store — nothing survives the process.
func OpenMem() (*DB, error) {
	p := C.kevy_open_mem()
	if p == nil {
		return nil, errors.New("kevy: open failed")
	}
	return &DB{p: p}, nil
}

// Fsync is the AOF fsync policy for OpenWith.
type Fsync uint8

const (
	// FsyncEverySec fsyncs once a second (the default — Redis appendfsync
	// everysec).
	FsyncEverySec Fsync = 0
	// FsyncAlways fsyncs on every write.
	FsyncAlways Fsync = 1
	// FsyncNo never fsyncs; the OS decides.
	FsyncNo Fsync = 2
)

// OpenOptions is the explicit open policy for OpenWith — the knobs Open
// locks to defaults. The zero value disables auto-rewrite entirely
// (RewritePct 0 is the off switch, as in Redis); start from
// DefaultOpenOptions for Open's exact defaults and override what you
// need. RewriteBytes and RewriteIntervalSecs are the absolute-size and
// staleness rewrite triggers (0 = off).
type OpenOptions struct {
	Fsync               Fsync  // AOF fsync policy
	Shards              uint32 // keyspace shards (0 = default, 1)
	RewritePct          uint32 // growth trigger, percent (0 = rule off)
	RewriteMinSize      uint64 // growth rule's minimum size gate
	RewriteBytes        uint64 // absolute-size trigger (0 = off)
	RewriteIntervalSecs uint64 // staleness trigger, seconds (0 = off)
}

// DefaultOpenOptions returns the exact defaults Open uses (the C header's
// KEVY_OPEN_OPTIONS_INIT): fsync everysec, growth rule at 100% over a
// 64 MiB floor, absolute-size and staleness triggers off.
func DefaultOpenOptions() OpenOptions {
	return OpenOptions{RewritePct: 100, RewriteMinSize: 64 << 20}
}

// OpenWith is Open with explicit options: durable at dir when dir is
// non-empty, in-memory when dir is "". A nil opts behaves exactly like
// Open / OpenMem.
func OpenWith(dir string, opts *OpenOptions) (*DB, error) {
	b := []byte(dir)
	var ptr *C.uint8_t
	if len(b) > 0 {
		ptr = (*C.uint8_t)(unsafe.Pointer(&b[0]))
	}
	var copts *C.KevyOpenOptions
	if opts != nil {
		copts = &C.KevyOpenOptions{
			fsync:                 C.uint8_t(opts.Fsync),
			shards:                C.uint32_t(opts.Shards),
			rewrite_pct:           C.uint32_t(opts.RewritePct),
			rewrite_min_size:      C.uint64_t(opts.RewriteMinSize),
			rewrite_bytes:         C.uint64_t(opts.RewriteBytes),
			rewrite_interval_secs: C.uint64_t(opts.RewriteIntervalSecs),
		}
	}
	p := C.kevy_open_with(ptr, C.size_t(len(b)), copts)
	runtime.KeepAlive(b)
	if p == nil {
		return nil, errors.New("kevy: open failed")
	}
	return &DB{p: p}, nil
}

// Close releases the store. The handle must not be used afterwards.
func (d *DB) Close() {
	if d.p != nil {
		C.kevy_close(d.p)
		d.p = nil
	}
}

// Shutdown flushes every shard's AOF with a REAL fsync, writes the feed
// continuity marker, then refuses every later write (reads stay
// available) — the deterministic teardown for a host's signal handler:
// Shutdown, then exit. Idempotent. On an I/O failure the store is still
// usable; retry or exit.
func (d *DB) Shutdown() error {
	if d.p == nil {
		return errors.New("kevy: closed handle")
	}
	switch rc := C.kevy_shutdown(d.p); rc {
	case 0:
		return nil
	case -2:
		return errors.New("kevy: shutdown flush failed (fsync/marker I/O error; store still usable — retry or exit)")
	default:
		return errors.New("kevy: kevy_shutdown misuse")
	}
}

// Cmd runs one command; args[0] is the verb. The error is non-nil only for
// ABI misuse — inspect the Reply for protocol-level errors.
func (d *DB) Cmd(args ...string) (Reply, error) {
	raw := make([][]byte, len(args))
	for i, a := range args {
		raw[i] = []byte(a)
	}
	return d.CmdBytes(raw...)
}

// CmdBytes is Cmd for binary-safe arguments — the universal command path
// through which every one of kevy's ~184 verbs is reachable.
func (d *DB) CmdBytes(args ...[]byte) (Reply, error) {
	if d.p == nil {
		return Reply{}, errors.New("kevy: closed handle")
	}
	if len(args) == 0 {
		return Reply{}, errors.New("kevy: empty argv")
	}
	// The argv array holds Go pointers, and cgo only lets a Go pointer
	// travel inside another Go allocation when the pointees are pinned.
	// Pinning keeps this zero-copy — kevy_cmd copies on the Rust side.
	var pin runtime.Pinner
	defer pin.Unpin()
	ptrs := make([]*C.uint8_t, len(args))
	lens := make([]C.size_t, len(args))
	for i, a := range args {
		if len(a) > 0 {
			p := &a[0]
			pin.Pin(p)
			ptrs[i] = (*C.uint8_t)(unsafe.Pointer(p))
		} else {
			// C sees a non-null pointer with length 0; any stable byte works.
			pin.Pin(&empty)
			ptrs[i] = (*C.uint8_t)(unsafe.Pointer(&empty))
		}
		lens[i] = C.size_t(len(a))
	}
	var out C.KevyBuf
	rc := C.kevy_cmd(d.p, C.size_t(len(args)), &ptrs[0], &lens[0], &out)
	runtime.KeepAlive(args)
	if rc != 0 {
		return Reply{}, errors.New("kevy: kevy_cmd misuse")
	}
	return takeReply(out)
}

var empty byte

// GetScalar is the scalar fast GET (no argv/RESP framing, contract §5.2).
// ok is false on a miss or an expired key.
func (d *DB) GetScalar(key []byte) (value []byte, ok bool, err error) {
	if d.p == nil {
		return nil, false, errors.New("kevy: closed handle")
	}
	var pin runtime.Pinner
	defer pin.Unpin()
	kp := keyPtr(key, &pin)
	var out C.KevyBuf
	// Zero-copy shared lane: a bulk value comes back as an Arc clone (no engine
	// copy); GoBytes makes the one copy into Go-owned memory, then the paired
	// shared free drops the Arc. Saves the engine's into_owned copy on big GETs.
	rc := C.kevy_get_shared(d.p, kp, C.size_t(len(key)), &out)
	runtime.KeepAlive(key)
	if rc < 0 {
		return nil, false, errors.New("kevy: kevy_get_shared misuse")
	}
	if rc == 0 {
		return nil, false, nil
	}
	v := C.GoBytes(unsafe.Pointer(out.ptr), C.int(out.len))
	C.kevy_buf_free_shared(out.ptr, out.len, out.cap)
	return v, true, nil
}

// GetView is the zero-copy read: it hands fn a []byte that VIEWS the value's
// bytes directly (an Arc refcount bump, no copy into Go memory), then releases
// the view when fn returns. This is the lane that beats a memory-mapped store
// on large reads — kevy_get_shared is O(1) regardless of value size, where an
// mmap Get still copies out. ok is false on a miss (fn is not called).
//
// The []byte passed to fn is valid ONLY for the duration of fn — it aliases
// engine memory freed on return. Do NOT retain it or hand it to a goroutine
// that outlives fn; copy it out (append/GetScalar) if you need to keep it. The
// scoping mirrors bbolt's `db.View` slice contract.
func (d *DB) GetView(key []byte, fn func(value []byte)) (ok bool, err error) {
	if d.p == nil {
		return false, errors.New("kevy: closed handle")
	}
	var pin runtime.Pinner
	defer pin.Unpin()
	kp := keyPtr(key, &pin)
	var out C.KevyBuf
	rc := C.kevy_get_shared(d.p, kp, C.size_t(len(key)), &out)
	runtime.KeepAlive(key)
	if rc < 0 {
		return false, errors.New("kevy: kevy_get_shared misuse")
	}
	if rc == 0 {
		return false, nil
	}
	// View the value in place; free the shared buffer (drops the Arc) after fn.
	if out.len > 0 {
		fn(unsafe.Slice((*byte)(unsafe.Pointer(out.ptr)), int(out.len)))
	} else {
		fn(nil)
	}
	C.kevy_buf_free_shared(out.ptr, out.len, out.cap)
	return true, nil
}

// SetScalar is the scalar fast SET (contract §5.2). ttlMs == 0 means no
// TTL.
func (d *DB) SetScalar(key, val []byte, ttlMs uint64) error {
	if d.p == nil {
		return errors.New("kevy: closed handle")
	}
	var pin runtime.Pinner
	defer pin.Unpin()
	kp := keyPtr(key, &pin)
	vp := keyPtr(val, &pin)
	rc := C.kevy_set(d.p, kp, C.size_t(len(key)), vp, C.size_t(len(val)), C.uint64_t(ttlMs))
	runtime.KeepAlive(key)
	runtime.KeepAlive(val)
	if rc < 0 {
		return errors.New("kevy: kevy_set misuse or storage error")
	}
	return nil
}

// SetMany applies len(keys) SETs in one FFI crossing — the batch-write path
// (contract §5.2, kevy_set_many). A loop of SetScalar pays the cgo boundary
// once per key; SetMany pays it once for the whole batch, closing most of the
// bulk-write gap to native embedded stores. keys and vals must be equal length.
// Durability is unchanged (each set appends to the AOF; EverySec/Always govern
// the fsync).
func (d *DB) SetMany(keys, vals [][]byte) error {
	if d.p == nil {
		return errors.New("kevy: closed handle")
	}
	if len(keys) != len(vals) {
		return errors.New("kevy: SetMany keys/vals length mismatch")
	}
	if len(keys) != len(vals) {
		return errors.New("kevy: SetMany keys/vals length mismatch")
	}
	n := len(keys)
	if n == 0 {
		return nil
	}
	// The key/val bytes are copied into a fixed ~1 MiB C arena and flushed in
	// byte-bounded chunks: every pointer handed to C then points at C memory
	// (storing Go pointers in C memory is unsafe — the GC neither scans nor
	// keeps them alive). One crossing amortizes a whole chunk — many ops for
	// small values, fewer for large (where the per-op crossing is already a
	// small fraction). Memory stays bounded regardless of batch size.
	return d.setManyChunked(keys, vals)
}

const setManyArenaBytes = 1 << 20 // ~1 MiB per crossing

func (d *DB) setManyChunked(keys, vals [][]byte) error {
	n := len(keys)
	arenaCap := setManyArenaBytes
	arena := C.malloc(C.size_t(arenaCap))
	kp := C.malloc(C.size_t(n) * C.size_t(unsafe.Sizeof(uintptr(0))))
	kl := C.malloc(C.size_t(n) * C.size_t(unsafe.Sizeof(C.size_t(0))))
	vp := C.malloc(C.size_t(n) * C.size_t(unsafe.Sizeof(uintptr(0))))
	vl := C.malloc(C.size_t(n) * C.size_t(unsafe.Sizeof(C.size_t(0))))
	defer C.free(arena)
	defer C.free(kp)
	defer C.free(kl)
	defer C.free(vp)
	defer C.free(vl)
	buf := unsafe.Slice((*byte)(arena), arenaCap)
	kpa := unsafe.Slice((**C.uint8_t)(kp), n)
	kla := unsafe.Slice((*C.size_t)(kl), n)
	vpa := unsafe.Slice((**C.uint8_t)(vp), n)
	vla := unsafe.Slice((*C.size_t)(vl), n)

	i, off, cnt := 0, 0, 0
	flush := func() error {
		if cnt == 0 {
			return nil
		}
		rc := C.kevy_set_many(d.p, C.size_t(cnt),
			(**C.uint8_t)(kp), (*C.size_t)(kl), (**C.uint8_t)(vp), (*C.size_t)(vl))
		if rc < 0 {
			return errors.New("kevy: kevy_set_many misuse or storage error")
		}
		off, cnt = 0, 0
		return nil
	}
	for i < n {
		need := len(keys[i]) + len(vals[i])
		if need > arenaCap {
			// One item larger than the arena: flush what's queued and set it
			// directly (SetScalar passes the pointer, no arena copy/limit).
			if err := flush(); err != nil {
				return err
			}
			if err := d.SetScalar(keys[i], vals[i], 0); err != nil {
				return err
			}
			i++
			continue
		}
		if cnt > 0 && off+need > arenaCap {
			if err := flush(); err != nil {
				return err
			}
			continue // re-check the same i against the fresh arena
		}
		kpa[cnt] = (*C.uint8_t)(unsafe.Add(unsafe.Pointer((*byte)(arena)), off))
		kla[cnt] = C.size_t(len(keys[i]))
		off += copy(buf[off:], keys[i])
		vpa[cnt] = (*C.uint8_t)(unsafe.Add(unsafe.Pointer((*byte)(arena)), off))
		vla[cnt] = C.size_t(len(vals[i]))
		off += copy(buf[off:], vals[i])
		cnt++
		i++
	}
	return flush()
}

// OpenReport is the boot-replay verdict: what an open restored — and what
// it could not. DroppedBytes > 0 or Corrupt means the store recovered LESS
// than its files held (the dropped region was quarantined next to the
// AOF): surface it as a startup health check instead of scraping the boot
// WARN line from stderr.
type OpenReport struct {
	// ReplayedCommands is the commands replayed from the AOF(s), summed
	// across shards.
	ReplayedCommands uint64
	// ReplayedBytes is the bytes actually replayed (the valid prefixes).
	ReplayedBytes uint64
	// ElapsedMs is the wall-clock time of the startup replay.
	ElapsedMs uint64
	// DroppedBytes is the bytes dropped past the last replayable frame
	// (quarantined on disk).
	DroppedBytes uint64
	// Corrupt is true when any shard's replay stopped at a corrupt frame.
	Corrupt bool
	// QuarantineCount is the quarantine files written by the open's repair.
	QuarantineCount uint32
}

// OpenReport returns this store's boot-replay verdict (see OpenReport, the
// type). An in-memory or fresh-dir open reports all zeros.
func (d *DB) OpenReport() (OpenReport, error) {
	if d.p == nil {
		return OpenReport{}, errors.New("kevy: closed handle")
	}
	var out C.KevyOpenReport
	if C.kevy_open_report(d.p, &out) != 0 {
		return OpenReport{}, errors.New("kevy: kevy_open_report misuse")
	}
	return OpenReport{
		ReplayedCommands: uint64(out.replayed_commands),
		ReplayedBytes:    uint64(out.replayed_bytes),
		ElapsedMs:        uint64(out.elapsed_ms),
		DroppedBytes:     uint64(out.dropped_bytes),
		Corrupt:          out.corrupt != 0,
		QuarantineCount:  uint32(out.quarantine_count),
	}, nil
}

// Version reports the engine version, e.g. "5.0.0".
func Version() string {
	return C.GoString(C.kevy_version())
}

// ABI reports the runtime C ABI version.
func ABI() uint32 { return uint32(C.kevy_abi()) }

func keyPtr(b []byte, pin *runtime.Pinner) *C.uint8_t {
	if len(b) == 0 {
		pin.Pin(&empty)
		return (*C.uint8_t)(unsafe.Pointer(&empty))
	}
	p := &b[0]
	pin.Pin(p)
	return (*C.uint8_t)(unsafe.Pointer(p))
}

func takeReply(buf C.KevyBuf) (Reply, error) {
	defer C.kevy_buf_free(buf.ptr, buf.len, buf.cap)
	if buf.len == 0 {
		return Reply{}, errors.New("kevy: empty reply")
	}
	// Parse directly over a zero-copy view of the C buffer instead of first
	// GoBytes-copying the whole reply: parseReply clones every []byte it
	// retains (cloneBytes), so nothing outlives this view, and the paired
	// kevy_buf_free below reclaims the buffer once decoding is done.
	raw := unsafe.Slice((*byte)(unsafe.Pointer(buf.ptr)), int(buf.len))
	r, err := decodeReply(raw)
	runtime.KeepAlive(buf)
	return r, err
}

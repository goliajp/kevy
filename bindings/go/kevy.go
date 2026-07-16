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

// Close releases the store. The handle must not be used afterwards.
func (d *DB) Close() {
	if d.p != nil {
		C.kevy_close(d.p)
		d.p = nil
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

// Version reports the engine version, e.g. "4.0.0".
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

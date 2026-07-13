// Package kevy embeds the kevy engine — the same engine the server runs —
// behind the C ABI in crates/kevy-ffi.
//
// One method carries the whole command surface: Cmd sends argv, the reply
// comes back parsed from RESP. A protocol error (-ERR …) is a Value with
// Kind == KindError, not a Go error: the engine answering "no" is a working
// engine. Go errors are reserved for ABI misuse and open failures.
//
//	db, err := kevy.Open("data/")
//	defer db.Close()
//	db.Cmd("SET", "k", "v")
//	v, _ := db.Cmd("GET", "k") // v.Str == "v"
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

// DB is an open embedded engine. Close it exactly once.
type DB struct {
	p *C.KevyDb
}

// Open opens a persistent store rooted at dir, replaying its log.
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
// ABI misuse — inspect the Value for protocol-level errors.
func (d *DB) Cmd(args ...string) (Value, error) {
	raw := make([][]byte, len(args))
	for i, a := range args {
		raw[i] = []byte(a)
	}
	return d.CmdBytes(raw...)
}

// CmdBytes is Cmd for binary-safe arguments.
func (d *DB) CmdBytes(args ...[]byte) (Value, error) {
	if d.p == nil {
		return Value{}, errors.New("kevy: closed handle")
	}
	if len(args) == 0 {
		return Value{}, errors.New("kevy: empty argv")
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
		return Value{}, errors.New("kevy: kevy_cmd misuse")
	}
	return takeBuf(out)
}

var empty byte

// Sub is a subscription handle. Poll Next; Close to unsubscribe.
type Sub struct {
	p *C.KevySub
}

// Subscribe opens a subscription on one channel.
func (d *DB) Subscribe(channel string) (*Sub, error) {
	return d.sub(channel, false)
}

// PSubscribe opens a subscription on one glob pattern.
func (d *DB) PSubscribe(pattern string) (*Sub, error) {
	return d.sub(pattern, true)
}

func (d *DB) sub(name string, pattern bool) (*Sub, error) {
	if d.p == nil {
		return nil, errors.New("kevy: closed handle")
	}
	b := []byte(name)
	var ptr *C.uint8_t
	if len(b) > 0 {
		ptr = (*C.uint8_t)(unsafe.Pointer(&b[0]))
	}
	var p *C.KevySub
	if pattern {
		p = C.kevy_psubscribe(d.p, ptr, C.size_t(len(b)))
	} else {
		p = C.kevy_subscribe(d.p, ptr, C.size_t(len(b)))
	}
	runtime.KeepAlive(b)
	if p == nil {
		return nil, errors.New("kevy: subscribe failed")
	}
	return &Sub{p: p}, nil
}

// Next drains one pending frame (message / pmessage / ack) without
// blocking. ok is false when nothing is queued.
func (s *Sub) Next() (v Value, ok bool, err error) {
	if s.p == nil {
		return Value{}, false, errors.New("kevy: closed subscription")
	}
	var out C.KevyBuf
	rc := C.kevy_sub_next(s.p, &out)
	if rc < 0 {
		return Value{}, false, errors.New("kevy: subscription misuse")
	}
	if rc == 0 {
		return Value{}, false, nil
	}
	val, err := takeBuf(out)
	return val, err == nil, err
}

// Close unsubscribes from everything this handle held.
func (s *Sub) Close() {
	if s.p != nil {
		C.kevy_sub_close(s.p)
		s.p = nil
	}
}

// Version reports the engine version, e.g. "4.0.0".
func Version() string {
	return C.GoString(C.kevy_version())
}

func takeBuf(buf C.KevyBuf) (Value, error) {
	defer C.kevy_buf_free(buf.ptr, buf.len, buf.cap)
	if buf.len == 0 {
		return Value{}, errors.New("kevy: empty reply")
	}
	raw := C.GoBytes(unsafe.Pointer(buf.ptr), C.int(buf.len))
	v, _, err := parseRESP(raw)
	return v, err
}

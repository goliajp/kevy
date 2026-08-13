//go:build kevy_embedded

package kevy

/*
#include <stdlib.h>
#include "kevy.h"
*/
import "C"

import (
	"errors"
	"runtime"
	"time"
)

// Sub is an embedded subscription handle over one channel or pattern —
// the minimal kevy-embedded pub/sub surface (contract §5.2). Poll with
// Next, block with Wait, and Close to unsubscribe. The higher-level,
// multi-channel Subscriber (contract §3.11) is built on top of these.
type Sub struct {
	p *C.KevySub
}

// Subscribe opens a subscription on one channel.
func (d *DB) Subscribe(channel string) (*Sub, error) {
	return d.subBytes([]byte(channel), false)
}

// PSubscribe opens a subscription on one glob pattern.
func (d *DB) PSubscribe(pattern string) (*Sub, error) {
	return d.subBytes([]byte(pattern), true)
}

func (d *DB) subBytes(name []byte, pattern bool) (*Sub, error) {
	if d.p == nil {
		return nil, errors.New("kevy: closed handle")
	}
	var pin runtime.Pinner
	defer pin.Unpin()
	ptr := keyPtr(name, &pin)
	var p *C.KevySub
	if pattern {
		p = C.kevy_psubscribe(d.p, ptr, C.size_t(len(name)))
	} else {
		p = C.kevy_subscribe(d.p, ptr, C.size_t(len(name)))
	}
	runtime.KeepAlive(name)
	if p == nil {
		return nil, errors.New("kevy: subscribe failed")
	}
	return &Sub{p: p}, nil
}

// Next drains one pending frame (message / pmessage / ack) without
// blocking. ok is false when nothing is queued.
func (s *Sub) Next() (r Reply, ok bool, err error) {
	if s.p == nil {
		return Reply{}, false, errors.New("kevy: closed subscription")
	}
	var out C.KevyBuf
	rc := C.kevy_sub_next(s.p, &out)
	if rc < 0 {
		return Reply{}, false, errors.New("kevy: subscription misuse")
	}
	if rc == 0 {
		return Reply{}, false, nil
	}
	val, err := takeReply(out)
	return val, err == nil, err
}

// Wait blocks up to timeout for one frame (timeout == 0 waits forever).
// ok is false on timeout. Parks the thread — no busy-poll.
func (s *Sub) Wait(timeout time.Duration) (r Reply, ok bool, err error) {
	if s.p == nil {
		return Reply{}, false, errors.New("kevy: closed subscription")
	}
	ms := uint64(0)
	if timeout > 0 {
		ms = uint64(timeout.Milliseconds())
		if ms == 0 {
			ms = 1 // sub-ms positive timeout must not collapse to "forever"
		}
	}
	var out C.KevyBuf
	rc := C.kevy_sub_wait(s.p, C.uint64_t(ms), &out)
	if rc < 0 {
		return Reply{}, false, errors.New("kevy: subscription closed")
	}
	if rc == 0 {
		return Reply{}, false, nil
	}
	val, err := takeReply(out)
	return val, err == nil, err
}

// Close unsubscribes from everything this handle held.
func (s *Sub) Close() {
	if s.p != nil {
		C.kevy_sub_close(s.p)
		s.p = nil
	}
}

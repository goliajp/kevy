//go:build kevy_embedded

package kevy

import (
	"bytes"
	"time"
)

// embSub backs an embedded Subscriber with one FFI subscription handle per
// channel/pattern (the C ABI's pub/sub is per-channel; contract §5.1). It
// polls the handles round-robin and blocks efficiently on the single-
// handle common case. The store is shared through the process registry so
// a Publish on the same URL reaches here.
type embSub struct {
	db      embStore
	key     string
	handles []subHandle
	rr      int
}

type subHandle struct {
	sub     embSubscription
	name    []byte
	pattern bool
}

func (e *embSub) add(names [][]byte, pattern bool) error {
	for _, n := range names {
		s, err := e.db.subBytes(n, pattern)
		if err != nil {
			return errClosed()
		}
		e.handles = append(e.handles, subHandle{sub: s, name: cloneBytes(n), pattern: pattern})
	}
	return nil
}

func (e *embSub) remove(names [][]byte, pattern bool) {
	keep := e.handles[:0]
	for _, h := range e.handles {
		drop := h.pattern == pattern && (len(names) == 0 || nameIn(names, h.name))
		if drop {
			h.sub.Close()
		} else {
			keep = append(keep, h)
		}
	}
	e.handles = keep
}

func (e *embSub) close() {
	for _, h := range e.handles {
		h.sub.Close()
	}
	e.handles = nil
	if e.db != nil {
		releaseStore(e.key, e.db)
		e.db = nil
	}
}

// recv blocks for the next frame across all handles. timeout == nil waits
// forever; a bounded timeout that elapses returns a TimedOut error.
func (e *embSub) recv(timeout *time.Duration) (Reply, error) {
	var end time.Time
	if timeout != nil {
		end = time.Now().Add(*timeout)
	}
	for {
		if r, ok, err := e.poll(); err != nil {
			return Reply{}, err
		} else if ok {
			return r, nil
		}
		if len(e.handles) == 1 {
			r, ok, err := e.blockOne(timeout, end)
			if err != nil || ok {
				return r, err
			}
			if timeout != nil {
				return Reply{}, &KevyError{Kind: KindTimedOut}
			}
			continue
		}
		if timeout != nil && !time.Now().Before(end) {
			return Reply{}, &KevyError{Kind: KindTimedOut}
		}
		time.Sleep(2 * time.Millisecond)
	}
}

// poll drains one queued frame from any handle, advancing the round-robin
// cursor so no handle is starved.
func (e *embSub) poll() (Reply, bool, error) {
	n := len(e.handles)
	for i := 0; i < n; i++ {
		h := e.handles[(e.rr+i)%n]
		r, ok, err := h.sub.Next()
		if err != nil {
			return Reply{}, false, errClosed()
		}
		if ok {
			e.rr = (e.rr + i + 1) % n
			return r, true, nil
		}
	}
	return Reply{}, false, nil
}

func (e *embSub) blockOne(timeout *time.Duration, end time.Time) (Reply, bool, error) {
	var wait time.Duration
	if timeout != nil {
		wait = time.Until(end)
		if wait <= 0 {
			return Reply{}, false, nil
		}
	}
	r, ok, err := e.handles[0].sub.Wait(wait)
	if err != nil {
		return Reply{}, false, errClosed()
	}
	return r, ok, nil
}

func nameIn(names [][]byte, n []byte) bool {
	for _, m := range names {
		if bytes.Equal(m, n) {
			return true
		}
	}
	return false
}

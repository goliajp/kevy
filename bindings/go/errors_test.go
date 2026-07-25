package kevy

import (
	"testing"
)

// Error-as-value / structured-error mapping (contract §2, §6).

func TestWrongTypeAndNotInteger(t *testing.T) {
	for _, bk := range bothBackends(t) {
		t.Run(bk.name, func(t *testing.T) {
			url, cleanup := bk.url(t)
			defer cleanup()
			c := mustConnect(t, url)

			_ = c.Set(bg, b("str"), b("hello"))
			// A list op on a string → Store(WrongType).
			_, err := c.LPush(bg, b("str"), b("x"))
			if se, ok := StoreErrorOf(err); !ok || se != StoreWrongType {
				t.Fatalf("LPush on string: want Store(WrongType), got %v", err)
			}
			// INCR on a non-numeric string → Store(NotInteger).
			_ = c.Set(bg, b("nan"), b("abc"))
			_, err = c.Incr(bg, b("nan"))
			if se, ok := StoreErrorOf(err); !ok || se != StoreNotInteger {
				t.Fatalf("INCR non-numeric: want Store(NotInteger), got %v", err)
			}
			// GET on a list must surface WRONGTYPE, not collapse to a miss.
			// The embedded scalar shared lane can't convey WRONGTYPE, so Get
			// falls back to the framed GET to preserve the typed error —
			// matching the remote backend.
			_, _ = c.LPush(bg, b("lst"), b("a"))
			_, _, err = c.Get(bg, b("lst"))
			if se, ok := StoreErrorOf(err); !ok || se != StoreWrongType {
				t.Fatalf("GET on list: want Store(WrongType), got %v", err)
			}
		})
	}
}

func TestGenericProtocolError(t *testing.T) {
	// An unknown verb / bad arity surfaces as Protocol with wire text
	// preserved, NOT a transport error. Remote path (raw Do).
	s := spawnServer(t)
	c := mustConnect(t, s.url())
	r, err := c.Do(bg, b("NOSUCHVERB"))
	if err != nil {
		t.Fatalf("Do transport error: %v", err)
	}
	if !r.IsError() {
		t.Fatalf("unknown verb should be an error reply, got %+v", r)
	}
	// The typed path maps it to a Protocol *KevyError with the text.
	_, err = c.IdxQueryRaw(bg, b("nope"), b("EQ"), b("1"))
	if !IsKind(err, KindProtocol) {
		t.Fatalf("want Protocol error, got %v", err)
	}
}

func TestEmbeddedRemoteOnlyUnsupported(t *testing.T) {
	c := mustConnect(t, "mem://unsupported-1")
	if _, err := c.IdxList(bg); !IsKind(err, KindUnsupported) {
		t.Fatalf("embedded IDX.LIST: want Unsupported, got %v", err)
	}
	if _, err := c.Multi(bg); !IsKind(err, KindUnsupported) {
		t.Fatalf("embedded MULTI: want Unsupported, got %v", err)
	}
	if _, err := c.Pipeline(bg, func(p *PipelineBuf) { p.Cmd(b("PING")) }); !IsKind(err, KindUnsupported) {
		t.Fatalf("embedded pipeline: want Unsupported, got %v", err)
	}
	if err := c.Watch(bg, b("k")); !IsKind(err, KindUnsupported) {
		t.Fatalf("embedded WATCH: want Unsupported, got %v", err)
	}
}

func TestServerCloseMidRead(t *testing.T) {
	// Dropping the server mid-connection surfaces Closed/Io, and a fresh
	// Connect can resume (contract §6 reconnect).
	s := spawnServer(t)
	c := mustConnect(t, s.url())
	if err := c.Set(bg, b("k"), b("v")); err != nil {
		t.Fatal(err)
	}
	_ = s.cmd.Process.Kill()
	_, _ = s.cmd.Process.Wait()
	// Next command must fail with a transport/closed error, not hang.
	_, _, err := c.Get(bg, b("k"))
	if err == nil {
		t.Fatal("expected error after server death")
	}
	if !IsKind(err, KindClosed) && !IsKind(err, KindIo) && !IsKind(err, KindTimedOut) {
		t.Fatalf("want Closed/Io/TimedOut, got %v", err)
	}
}

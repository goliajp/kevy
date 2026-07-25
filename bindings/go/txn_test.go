package kevy

import (
	"testing"
)

// Transactions + pipeline (contract §3.12/§3.13, §6). Remote-only.

func TestTransactionExec(t *testing.T) {
	s := spawnServer(t)
	c := mustConnect(t, s.url())

	txn, err := c.Multi(bg)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := txn.Set(b("a"), b("1")); err != nil {
		t.Fatal(err)
	}
	if _, err := txn.Incr(b("n")); err != nil {
		t.Fatal(err)
	}
	if _, err := txn.Get(b("a")); err != nil {
		t.Fatal(err)
	}
	replies, err := txn.Exec()
	if err != nil {
		t.Fatal(err)
	}
	if len(replies) != 3 {
		t.Fatalf("EXEC replies=%d want 3", len(replies))
	}
	if replies[0].Kind != KindSimple || replies[1].Int != 1 || replies[2].Str() != "1" {
		t.Fatalf("replies=%+v", replies)
	}
}

func TestTransactionTypedCursor(t *testing.T) {
	s := spawnServer(t)
	c := mustConnect(t, s.url())
	txn, _ := c.Multi(bg)
	_, _ = txn.Set(b("a"), b("v"))
	_, _ = txn.Incr(b("ctr"))
	_, _ = txn.Get(b("a"))
	cur, err := txn.ExecTyped()
	if err != nil {
		t.Fatal(err)
	}
	if err := cur.NextOK(); err != nil {
		t.Fatalf("NextOK: %v", err)
	}
	n, err := cur.NextInt()
	if err != nil || n != 1 {
		t.Fatalf("NextInt=%d %v", n, err)
	}
	v, ok, err := cur.NextBulk()
	if err != nil || !ok || string(v) != "v" {
		t.Fatalf("NextBulk=%q %v %v", v, ok, err)
	}
	if err := cur.ExpectEmpty(); err != nil {
		t.Fatalf("ExpectEmpty arity gate: %v", err)
	}
}

func TestTransactionWatchAbort(t *testing.T) {
	s := spawnServer(t)
	c := mustConnect(t, s.url())
	other := mustConnect(t, s.url())

	if err := c.Watch(bg, b("wk")); err != nil {
		t.Fatal(err)
	}
	// A concurrent modify on the watched key from another connection.
	if err := other.Set(bg, b("wk"), b("changed")); err != nil {
		t.Fatal(err)
	}
	txn, _ := c.Multi(bg)
	_, _ = txn.Set(b("wk"), b("mine"))
	_, ok, err := txn.ExecWatched()
	if err != nil {
		t.Fatal(err)
	}
	if ok {
		t.Fatal("WATCH violation should abort (ExecWatched ok=false)")
	}

	// exec_typed on abort → Protocol error.
	if err := c.Watch(bg, b("wk")); err != nil {
		t.Fatal(err)
	}
	_ = other.Set(bg, b("wk"), b("again"))
	txn2, _ := c.Multi(bg)
	_, _ = txn2.Incr(b("wk2"))
	if _, err := txn2.ExecTyped(); !IsKind(err, KindProtocol) {
		t.Fatalf("ExecTyped on abort should be Protocol, got %v", err)
	}
}

func TestTransactionImplicitDiscard(t *testing.T) {
	s := spawnServer(t)
	c := mustConnect(t, s.url())
	txn, err := c.Multi(bg)
	if err != nil {
		t.Fatal(err)
	}
	_, _ = txn.Set(b("x"), b("1"))
	// Abandon without Exec/Discard → Close sends implicit DISCARD, leaving
	// the socket usable for ordinary commands.
	txn.Close()
	if err := c.Set(bg, b("y"), b("2")); err != nil {
		t.Fatalf("socket stuck in MULTI after abandon: %v", err)
	}
	if v, ok, _ := c.Get(bg, b("y")); !ok || string(v) != "2" {
		t.Fatalf("post-discard command failed: %q %v", v, ok)
	}
	// The abandoned SET must NOT have applied.
	if _, ok, _ := c.Get(bg, b("x")); ok {
		t.Fatal("abandoned transaction leaked a write")
	}
}

func TestPipeline(t *testing.T) {
	s := spawnServer(t)
	c := mustConnect(t, s.url())

	replies, err := c.Pipeline(bg, func(p *PipelineBuf) {
		p.Cmd(b("SET"), b("a"), b("1")).Cmd(b("INCR"), b("n")).Cmd(b("GET"), b("a"))
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(replies) != 3 || replies[1].Int != 1 || replies[2].Str() != "1" {
		t.Fatalf("pipeline replies=%+v", replies)
	}

	// Per-command error lands inline as Reply::Error; batch not aborted.
	replies, err = c.Pipeline(bg, func(p *PipelineBuf) {
		p.Cmd(b("SET"), b("s"), b("str")).Cmd(b("INCR"), b("s")).Cmd(b("GET"), b("s"))
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(replies) != 3 || !replies[1].IsError() || replies[2].Str() != "str" {
		t.Fatalf("inline error batch=%+v", replies)
	}

	// Empty batch → no wire I/O.
	empty, err := c.Pipeline(bg, func(p *PipelineBuf) {})
	if err != nil || len(empty) != 0 {
		t.Fatalf("empty batch=%v %v", empty, err)
	}

	// Empty-argv poisons → InvalidInput.
	_, err = c.Pipeline(bg, func(p *PipelineBuf) { p.Cmd() })
	if !IsKind(err, KindInvalidInput) {
		t.Fatalf("empty argv should be InvalidInput, got %v", err)
	}
}

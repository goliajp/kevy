package kevy

// The Go smoke — the same contract as examples/c/smoke.c: commands, a
// protocol error as data, pub/sub round trip, and a kill-and-reopen
// durability check. ffigate runs this on every push.

import (
	"path/filepath"
	"testing"
)

func TestSmoke(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "data")

	db, err := Open(dir)
	if err != nil {
		t.Fatal(err)
	}

	if v, _ := db.Cmd("SET", "smoke:go", "v1"); v.Str() != "OK" {
		t.Fatalf("SET: %+v", v)
	}
	if v, _ := db.Cmd("GET", "smoke:go"); v.Str() != "v1" {
		t.Fatalf("GET: %+v", v)
	}
	if v, _ := db.Cmd("NOSUCHVERB"); !v.IsError() {
		t.Fatalf("unknown verb should be a protocol error, got %+v", v)
	}

	sub, err := db.Subscribe("c1")
	if err != nil {
		t.Fatal(err)
	}
	if _, ok, _ := sub.Next(); !ok {
		t.Fatal("missing subscribe ack")
	}
	if v, _ := db.Cmd("PUBLISH", "c1", "hello"); v.Int != 1 {
		t.Fatalf("PUBLISH: %+v", v)
	}
	frame, ok, err := sub.Next()
	if err != nil || !ok {
		t.Fatalf("missing message frame: %v", err)
	}
	if len(frame.Array) != 3 || frame.Array[0].Str() != "message" ||
		frame.Array[1].Str() != "c1" || frame.Array[2].Str() != "hello" {
		t.Fatalf("frame: %+v", frame)
	}
	if _, ok, _ := sub.Next(); ok {
		t.Fatal("queue should be drained")
	}
	sub.Close()

	// Durability: close, reopen the same dir, the key survived.
	db.Close()
	db, err = Open(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if v, _ := db.Cmd("GET", "smoke:go"); v.Str() != "v1" {
		t.Fatalf("GET after reopen: %+v", v)
	}
	if v, _ := db.Cmd("DEL", "smoke:go"); v.Int != 1 {
		t.Fatalf("DEL: %+v", v)
	}
}

func TestVersion(t *testing.T) {
	if Version() == "" {
		t.Fatal("empty version")
	}
}

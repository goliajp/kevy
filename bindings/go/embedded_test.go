package kevy

import (
	"path/filepath"
	"testing"
	"time"
)

// Embedded store contract (contract §5.2, §5.3, §6).

func TestEmbeddedScalarAndCmd(t *testing.T) {
	db, err := OpenMem()
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()

	// cmd(argv) runs an arbitrary verb and returns a parseable reply.
	if r, _ := db.Cmd("SET", "k", "v"); r.Str() != "OK" {
		t.Fatalf("Cmd SET: %+v", r)
	}
	// scalar fast paths.
	if err := db.SetScalar([]byte("s"), []byte("fast"), 0); err != nil {
		t.Fatal(err)
	}
	v, ok, err := db.GetScalar([]byte("s"))
	if err != nil || !ok || string(v) != "fast" {
		t.Fatalf("GetScalar=%q %v %v", v, ok, err)
	}
	if _, ok, _ := db.GetScalar([]byte("absent")); ok {
		t.Fatal("GetScalar(absent) claimed hit")
	}
	// scalar SET with TTL.
	if err := db.SetScalar([]byte("t"), []byte("x"), 30_000); err != nil {
		t.Fatal(err)
	}
	if r, _ := db.Cmd("PTTL", "t"); r.Int <= 0 || r.Int > 30_000 {
		t.Fatalf("scalar TTL PTTL=%d", r.Int)
	}

	// subscribe → poll Next + blocking Wait yield RESP frames.
	sub, err := db.Subscribe("c")
	if err != nil {
		t.Fatal(err)
	}
	defer sub.Close()
	if _, ok, _ := sub.Next(); !ok {
		t.Fatal("missing subscribe ack via Next")
	}
	if r, _ := db.Cmd("PUBLISH", "c", "hi"); r.Int != 1 {
		t.Fatalf("PUBLISH count=%d", r.Int)
	}
	frame, ok, err := sub.Wait(time.Second)
	if err != nil || !ok || len(frame.Array) != 3 || frame.Array[0].Str() != "message" {
		t.Fatalf("Wait frame=%+v %v %v", frame, ok, err)
	}
}

func TestEmbeddedPersistenceReopen(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "data")

	// Write via a file:// client, then close (drops the last registry
	// handle → store flushed + evicted).
	c := mustConnectNoCleanup(t, "file://"+dir)
	if err := c.Set(bg, b("durable"), b("survives")); err != nil {
		t.Fatal(err)
	}
	if _, err := c.HSet(bg, b("h"), b("f"), b("hv")); err != nil {
		t.Fatal(err)
	}
	c.Close()

	// Reopen the same dir: snapshot + AOF replay restores state.
	c2 := mustConnect(t, "file://"+dir)
	v, ok, err := c2.Get(bg, b("durable"))
	if err != nil || !ok || string(v) != "survives" {
		t.Fatalf("value did not survive reopen: %q %v %v", v, ok, err)
	}
	hv, ok, err := c2.HGet(bg, b("h"), b("f"))
	if err != nil || !ok || string(hv) != "hv" {
		t.Fatalf("hash did not survive reopen: %q %v %v", hv, ok, err)
	}
}

func TestEmbeddedOpenReport(t *testing.T) {
	// A clean in-memory open has nothing to replay: all zeros.
	mem, err := OpenMem()
	if err != nil {
		t.Fatal(err)
	}
	defer mem.Close()
	if r, err := mem.OpenReport(); err != nil || r != (OpenReport{}) {
		t.Fatalf("mem open report=%+v %v, want zeros", r, err)
	}

	// A reopen over real files replays them — the report says how much.
	dir := filepath.Join(t.TempDir(), "data")
	db, err := Open(dir)
	if err != nil {
		t.Fatal(err)
	}
	if err := db.SetScalar([]byte("k"), []byte("v"), 0); err != nil {
		t.Fatal(err)
	}
	db.Close()
	db2, err := Open(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer db2.Close()
	r, err := db2.OpenReport()
	if err != nil || r.ReplayedCommands == 0 || r.ReplayedBytes == 0 {
		t.Fatalf("reopen report=%+v %v, want replayed > 0", r, err)
	}
	if r.DroppedBytes != 0 || r.Corrupt || r.QuarantineCount != 0 {
		t.Fatalf("clean reopen flagged loss: %+v", r)
	}
}

func TestEmbeddedOpenWith(t *testing.T) {
	// Empty dir + nil opts = a plain in-memory open.
	mem, err := OpenWith("", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer mem.Close()
	if err := mem.SetScalar([]byte("k"), []byte("v"), 0); err != nil {
		t.Fatal(err)
	}
	if v, ok, _ := mem.GetScalar([]byte("k")); !ok || string(v) != "v" {
		t.Fatalf("mem OpenWith GetScalar=%q %v", v, ok)
	}

	// Explicit options round-trip on a durable dir.
	dir := filepath.Join(t.TempDir(), "data")
	opts := DefaultOpenOptions()
	opts.Fsync = FsyncAlways
	opts.Shards = 2
	opts.RewriteBytes = 1 << 30
	opts.RewriteIntervalSecs = 3600
	db, err := OpenWith(dir, &opts)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if err := db.SetScalar([]byte("opt"), []byte("set"), 0); err != nil {
		t.Fatal(err)
	}
	if v, ok, _ := db.GetScalar([]byte("opt")); !ok || string(v) != "set" {
		t.Fatalf("OpenWith GetScalar=%q %v", v, ok)
	}
}

func TestEmbeddedShutdown(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "data")
	db, err := OpenWith(dir, nil)
	if err != nil {
		t.Fatal(err)
	}
	if err := db.SetScalar([]byte("k"), []byte("pre-shutdown"), 0); err != nil {
		t.Fatal(err)
	}

	// The deterministic teardown: flushed + fsynced, idempotent.
	if err := db.Shutdown(); err != nil {
		t.Fatalf("Shutdown: %v", err)
	}
	if err := db.Shutdown(); err != nil {
		t.Fatalf("second Shutdown: %v", err)
	}

	// Later writes are refused; reads stay available.
	if err := db.SetScalar([]byte("k2"), []byte("x"), 0); err == nil {
		t.Fatal("write after Shutdown succeeded")
	}
	if v, ok, err := db.GetScalar([]byte("k")); err != nil || !ok || string(v) != "pre-shutdown" {
		t.Fatalf("read after Shutdown=%q %v %v", v, ok, err)
	}
	db.Close()

	// A reopen sees the pre-shutdown writes.
	db2, err := Open(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer db2.Close()
	if v, ok, err := db2.GetScalar([]byte("k")); err != nil || !ok || string(v) != "pre-shutdown" {
		t.Fatalf("value did not survive shutdown+reopen: %q %v %v", v, ok, err)
	}
}

func mustConnectNoCleanup(t *testing.T, url string) *Client {
	t.Helper()
	c, err := Connect(url)
	if err != nil {
		t.Fatalf("Connect(%q): %v", url, err)
	}
	return c
}

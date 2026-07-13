package kevy

import (
	"testing"
	"time"
)

// The typed surface exercised end to end, including the opinion that a
// protocol error is a Go error here (unlike Cmd, where it is data).
func TestTypedSurface(t *testing.T) {
	db, err := OpenMem()
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()

	if err := db.Set("k", []byte("v")); err != nil {
		t.Fatal(err)
	}
	v, ok, err := db.Get("k")
	if err != nil || !ok || string(v) != "v" {
		t.Fatalf("Get: %q %v %v", v, ok, err)
	}
	if _, ok, _ := db.Get("absent"); ok {
		t.Fatal("Get(absent) claimed a hit")
	}

	if err := db.SetEx("tmp", []byte("x"), 30*time.Second); err != nil {
		t.Fatal(err)
	}
	ttl, err := db.PTTL("tmp")
	if err != nil || ttl <= 0 || ttl > 30_000 {
		t.Fatalf("PTTL: %d %v", ttl, err)
	}

	if n, _ := db.Incr("hits"); n != 1 {
		t.Fatalf("Incr: %d", n)
	}
	if n, _ := db.IncrBy("hits", 10); n != 11 {
		t.Fatalf("IncrBy: %d", n)
	}

	// Typed layer's opinion: WRONGTYPE surfaces as a Go error.
	if _, err := db.Incr("k"); err == nil {
		t.Fatal("Incr on a plain string should error")
	}

	if got, _ := db.MGet("k", "absent", "tmp"); string(got[0]) != "v" ||
		got[1] != nil || string(got[2]) != "x" {
		t.Fatalf("MGet: %q", got)
	}

	if kind, _ := db.TypeOf("k"); kind != "string" {
		t.Fatalf("TypeOf: %q", kind)
	}
	if ok, _ := db.Expire("k", time.Minute); !ok {
		t.Fatal("Expire claimed miss")
	}
	if keys, _ := db.Keys("*"); len(keys) != 3 {
		t.Fatalf("Keys: %v", keys)
	}
	if n, _ := db.DBSize(); n != 3 {
		t.Fatalf("DBSize: %d", n)
	}
	if n, _ := db.Del("k", "absent"); n != 1 {
		t.Fatalf("Del: %d", n)
	}
	if err := db.FlushAll(); err != nil {
		t.Fatal(err)
	}
	if n, _ := db.DBSize(); n != 0 {
		t.Fatalf("DBSize after FlushAll: %d", n)
	}
}

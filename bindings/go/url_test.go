package kevy

import (
	"path/filepath"
	"testing"
)

// Connection & URL routing conformance (contract §6).

func TestURLRejections(t *testing.T) {
	// Embedded-only: it opens the in-process engine directly.
	if openEmbeddedStore == nil {
		t.Skip("no embedded engine in this build")
	}
	cases := []struct {
		url  string
		kind ErrorKind
	}{
		{"rediss://h:6379", KindUnsupported},
		{"kevys://h:6379", KindUnsupported},
		{"redis://user:pass@h:6379", KindUnsupported},
		{"memcached://h:11211", KindInvalidInput},
		{"file://", KindInvalidInput},
		{"noscheme", KindInvalidInput},
		{"kevy://:6379", KindInvalidInput},
		{"kevy://h:notaport", KindInvalidInput},
	}
	for _, tc := range cases {
		_, err := Connect(tc.url)
		if err == nil {
			t.Errorf("%s: expected error", tc.url)
			continue
		}
		if !IsKind(err, tc.kind) {
			t.Errorf("%s: expected kind %d, got %v", tc.url, tc.kind, err)
		}
	}
}

func TestMemAnonymousIsolated(t *testing.T) {
	// Embedded-only: it opens the in-process engine directly.
	if openEmbeddedStore == nil {
		t.Skip("no embedded engine in this build")
	}
	a := mustConnect(t, "mem://")
	bcli := mustConnect(t, "mem://")
	if err := a.Set(bg, b("k"), b("va")); err != nil {
		t.Fatal(err)
	}
	// A second anonymous mem:// is a separate store.
	if _, ok, _ := bcli.Get(bg, b("k")); ok {
		t.Fatal("anonymous mem:// leaked across connects")
	}
}

func TestMemNamedShares(t *testing.T) {
	// Embedded-only: it opens the in-process engine directly.
	if openEmbeddedStore == nil {
		t.Skip("no embedded engine in this build")
	}
	url := "mem://shared-bus-1"
	a := mustConnect(t, url)
	bcli := mustConnect(t, url)
	if err := a.Set(bg, b("k"), b("shared")); err != nil {
		t.Fatal(err)
	}
	v, ok, err := bcli.Get(bg, b("k"))
	if err != nil || !ok || string(v) != "shared" {
		t.Fatalf("named mem:// did not share store: %q %v %v", v, ok, err)
	}
}

func TestFileShares(t *testing.T) {
	// Embedded-only: it opens the in-process engine directly.
	if openEmbeddedStore == nil {
		t.Skip("no embedded engine in this build")
	}
	dir := filepath.Join(t.TempDir(), "data")
	url := "file://" + dir
	a := mustConnect(t, url)
	bcli := mustConnect(t, url)
	if err := a.Set(bg, b("k"), b("onfile")); err != nil {
		t.Fatal(err)
	}
	v, ok, err := bcli.Get(bg, b("k"))
	if err != nil || !ok || string(v) != "onfile" {
		t.Fatalf("file:// did not share store: %q %v %v", v, ok, err)
	}
}

func TestRegistryEvictsOnLastClose(t *testing.T) {
	// Embedded-only: it opens the in-process engine directly.
	if openEmbeddedStore == nil {
		t.Skip("no embedded engine in this build")
	}
	url := "mem://evict-1"
	a, err := Connect(url)
	if err != nil {
		t.Fatal(err)
	}
	_ = a.Set(bg, b("k"), b("v1"))
	a.Close() // last handle → store evicted
	// A fresh connect for the same URL must get a new, empty store.
	c2 := mustConnect(t, url)
	if _, ok, _ := c2.Get(bg, b("k")); ok {
		t.Fatal("registry did not evict store after last close")
	}
}

func TestRemoteSelect(t *testing.T) {
	s := spawnServer(t)
	// kevy://…/0 issues SELECT 0 (DB 0 supported) → connects fine.
	c, err := Connect(s.url() + "/0")
	if err != nil {
		t.Fatalf("SELECT 0 should succeed: %v", err)
	}
	c.Close()
	// tcp:// does no SELECT (raw) — also connects.
	tcpURL := "tcp://127.0.0.1:" + itoa(s.port)
	c2, err := Connect(tcpURL)
	if err != nil {
		t.Fatalf("tcp:// connect: %v", err)
	}
	c2.Close()
}

func itoa(n int) string { return string(itob(int64(n))) }

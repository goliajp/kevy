package kevy

import (
	"testing"
	"time"
)

// waitIdxReady polls IDX.LIST until the named index reports state=ready.
func waitIdxReady(t *testing.T, c *Client, name string) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		infos, err := c.IdxList(bg)
		if err != nil {
			t.Fatal(err)
		}
		for _, in := range infos {
			if string(in.Name) == name && in.State == "ready" {
				return
			}
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatalf("index %q never became ready", name)
}

// Declarative indexes IDX.* (contract §3.8, §6). Remote-only.

func TestIdxRangeAndPaging(t *testing.T) {
	s := spawnServer(t)
	c := mustConnect(t, s.url())

	if err := c.IdxCreateRange(bg, b("byage"), b("user:"), b("age"), IdxI64); err != nil {
		t.Fatal(err)
	}
	// Populate rows the index covers.
	for i, age := range []string{"21", "22", "23", "24", "25"} {
		key := "user:" + string(rune('a'+i))
		if _, err := c.HSet(bg, b(key), b("age"), b(age)); err != nil {
			t.Fatal(err)
		}
	}

	waitIdxReady(t, c, "byage")

	// idx_list parses IdxInfo (incl. unknown-label skip).
	infos, err := c.IdxList(bg)
	if err != nil {
		t.Fatal(err)
	}
	found := false
	for _, in := range infos {
		if string(in.Name) == "byage" {
			found = true
			if in.Kind != "range" {
				t.Fatalf("IdxInfo kind=%q", in.Kind)
			}
		}
	}
	if !found {
		t.Fatalf("byage not in IDX.LIST: %+v", infos)
	}

	// Range query with paging: LIMIT 2 pages through 5 rows [21..25].
	seen := 0
	var cursor []byte
	for {
		page, perr := c.IdxQueryRange(bg, b("byage"), b("0"), b("100"), 2, cursor)
		if perr != nil {
			t.Fatal(perr)
		}
		seen += len(page.Rows)
		if page.Cursor == nil {
			break
		}
		cursor = page.Cursor
		if seen > 10 {
			t.Fatal("paging did not terminate")
		}
	}
	if seen != 5 {
		t.Fatalf("range paging saw %d rows, want 5", seen)
	}

	// EQ point lookup.
	page, err := c.IdxQueryEq(bg, b("byage"), b("23"), 10)
	if err != nil {
		t.Fatal(err)
	}
	if len(page.Rows) != 1 || string(page.Rows[0].Value) != "23" {
		t.Fatalf("EQ page=%+v", page.Rows)
	}

	// drop returns existed.
	if ok, _ := c.IdxDrop(bg, b("byage")); !ok {
		t.Fatal("IdxDrop should report existed")
	}
	if ok, _ := c.IdxDrop(bg, b("byage")); ok {
		t.Fatal("second IdxDrop should report gone")
	}
}

func TestIdxEmbeddedUnsupported(t *testing.T) {
	c := mustConnect(t, "mem://idx-bus")
	if _, err := c.IdxList(bg); !IsKind(err, KindUnsupported) {
		t.Fatalf("embedded IdxList: %v", err)
	}
	if err := c.IdxCreateRange(bg, b("i"), b("u:"), b("age"), IdxI64); !IsKind(err, KindUnsupported) {
		t.Fatalf("embedded IdxCreateRange: %v", err)
	}
}

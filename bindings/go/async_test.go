package kevy

import (
	"context"
	"testing"
)

// Sync + async faces exist on ONE client and agree (contract §1.4, §6).

func TestAsyncAgreesWithSync(t *testing.T) {
	for _, bk := range bothBackends(t) {
		t.Run(bk.name, func(t *testing.T) {
			url, cleanup := bk.url(t)
			defer cleanup()
			c := mustConnect(t, url)
			a := c.Async()

			// Async Set/Get resolve to the same result as the blocking face.
			if _, err := a.Set(bg, b("k"), b("async")).Await(bg); err != nil {
				t.Fatal(err)
			}
			gr, err := a.Get(bg, b("k")).Await(bg)
			if err != nil || !gr.OK || string(gr.Value) != "async" {
				t.Fatalf("async Get=%+v %v", gr, err)
			}
			// The blocking face sees the same store state.
			v, ok, err := c.Get(bg, b("k"))
			if err != nil || !ok || string(v) != "async" {
				t.Fatalf("sync sees=%q %v %v", v, ok, err)
			}

			// Async Incr agrees with sync Incr.
			n1, err := a.Incr(bg, b("ctr")).Await(bg)
			if err != nil {
				t.Fatal(err)
			}
			n2, err := c.Incr(bg, b("ctr"))
			if err != nil {
				t.Fatal(err)
			}
			if n1 != 1 || n2 != 2 {
				t.Fatalf("async/sync Incr disagree: %d then %d", n1, n2)
			}

			// Generic async escape hatch resolves any blocking op.
			dbsize, err := GoAsync(a, bg, func(ctx context.Context, cl *Client) (int64, error) {
				return cl.DBSize(ctx)
			}).Await(bg)
			if err != nil || dbsize < 1 {
				t.Fatalf("GoAsync DBSize=%d %v", dbsize, err)
			}
		})
	}
}

package kevy

import (
	"testing"
	"time"
)

// Blocking pops (contract §3.14, §6).

func TestBlockingPops(t *testing.T) {
	for _, bk := range bothBackends(t) {
		t.Run(bk.name, func(t *testing.T) {
			url, cleanup := bk.url(t)
			defer cleanup()
			c := mustConnect(t, url)

			_, _ = c.RPush(bg, b("q"), b("a"), b("b"))
			d := time.Second
			hit, ok, err := c.BLPop(bg, [][]byte{b("q")}, &d)
			if err != nil || !ok || string(hit.Key) != "q" || string(hit.Value) != "a" {
				t.Fatalf("BLPop=%+v %v %v", hit, ok, err)
			}
			hit, ok, err = c.BRPop(bg, [][]byte{b("q")}, &d)
			if err != nil || !ok || string(hit.Value) != "b" {
				t.Fatalf("BRPop=%+v %v %v", hit, ok, err)
			}
			// Empty list with a short timeout → no hit.
			short := 40 * time.Millisecond
			_, ok, err = c.BLPop(bg, [][]byte{b("empty")}, &short)
			if err != nil || ok {
				t.Fatalf("BLPop timeout should miss: ok=%v err=%v", ok, err)
			}

			// bzpopmin lowest score.
			_, _ = c.ZAdd(bg, b("z"), ZMember{2, b("hi")}, ZMember{1, b("lo")})
			zh, ok, err := c.BZPopMin(bg, [][]byte{b("z")}, &d)
			if err != nil || !ok || string(zh.Member) != "lo" || zh.Score != 1 {
				t.Fatalf("BZPopMin=%+v %v %v", zh, ok, err)
			}

			// Some(0) and empty keys → InvalidInput.
			zero := time.Duration(0)
			if _, _, err := c.BLPop(bg, [][]byte{b("q")}, &zero); !IsKind(err, KindInvalidInput) {
				t.Fatalf("Some(0) should be InvalidInput, got %v", err)
			}
			if _, _, err := c.BLPop(bg, nil, &d); !IsKind(err, KindInvalidInput) {
				t.Fatalf("empty keys should be InvalidInput, got %v", err)
			}
		})
	}
}

func TestBlockingPopWakesOnPush(t *testing.T) {
	// A real block that is released by a concurrent push on the same
	// (shared) store / server.
	for _, bk := range bothBackends(t) {
		t.Run(bk.name, func(t *testing.T) {
			url, cleanup := bk.url(t)
			defer cleanup()
			consumer := mustConnect(t, url)
			producer := mustConnect(t, url)

			done := make(chan KV, 1)
			go func() {
				d := 3 * time.Second
				hit, _, _ := consumer.BLPop(bg, [][]byte{b("bq")}, &d)
				done <- hit
			}()
			time.Sleep(80 * time.Millisecond)
			if _, err := producer.RPush(bg, b("bq"), b("payload")); err != nil {
				t.Fatal(err)
			}
			select {
			case hit := <-done:
				if string(hit.Value) != "payload" {
					t.Fatalf("woke with wrong value: %q", hit.Value)
				}
			case <-time.After(3 * time.Second):
				t.Fatal("BLPop did not wake on push")
			}
		})
	}
}

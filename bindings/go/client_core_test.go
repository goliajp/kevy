package kevy

import (
	"testing"
	"time"
)

// Core KV + collections + zalgebra + hash-field TTL, exercised against
// both the embedded and remote backends (contract §3.1–§3.7, §6).

func TestCoreKV(t *testing.T) {
	for _, bk := range bothBackends(t) {
		t.Run(bk.name, func(t *testing.T) {
			url, cleanup := bk.url(t)
			defer cleanup()
			c := mustConnect(t, url)

			if err := c.Set(bg, b("k"), b("v")); err != nil {
				t.Fatal(err)
			}
			if v, ok, _ := c.Get(bg, b("k")); !ok || string(v) != "v" {
				t.Fatalf("Get: %q %v", v, ok)
			}
			if _, ok, _ := c.Get(bg, b("absent")); ok {
				t.Fatal("Get(absent) claimed hit")
			}
			if n, _ := c.Incr(bg, b("hits")); n != 1 {
				t.Fatalf("Incr=%d", n)
			}
			if n, _ := c.IncrBy(bg, b("hits"), 10); n != 11 {
				t.Fatalf("IncrBy=%d", n)
			}
			if n, _ := c.Exists(bg, b("k"), b("k"), b("absent")); n != 2 {
				t.Fatalf("Exists (repeated counts each)=%d", n)
			}
			if n, _ := c.Del(bg, b("k"), b("absent")); n != 1 {
				t.Fatalf("Del=%d", n)
			}

			// TTL family: -2 no key, -1 no TTL.
			if ms, _ := c.TTLMs(bg, b("nope")); ms != -2 {
				t.Fatalf("PTTL no key=%d", ms)
			}
			_ = c.Set(bg, b("t"), b("x"))
			if ms, _ := c.TTLMs(bg, b("t")); ms != -1 {
				t.Fatalf("PTTL no ttl=%d", ms)
			}
			if err := c.SetWithTTL(bg, b("t2"), b("x"), 30*time.Second); err != nil {
				t.Fatal(err)
			}
			if ms, _ := c.TTLMs(bg, b("t2")); ms <= 0 || ms > 30_000 {
				t.Fatalf("SetWithTTL PTTL=%d", ms)
			}
			ok, _ := c.Expire(bg, b("t"), time.Minute)
			if !ok {
				t.Fatal("Expire claimed miss")
			}
			if removed, _ := c.Persist(bg, b("t")); !removed {
				t.Fatal("Persist claimed no TTL")
			}

			if kind, _ := c.TypeOf(bg, b("t")); kind != "string" {
				t.Fatalf("TypeOf=%q", kind)
			}
			if kind, _ := c.TypeOf(bg, b("ghost")); kind != "none" {
				t.Fatalf("TypeOf(none)=%q", kind)
			}

			// mget order + nulls; mset atomic.
			if err := c.MSet(bg, b("a"), b("1"), b("bb"), b("2")); err != nil {
				t.Fatal(err)
			}
			got, _ := c.MGet(bg, b("a"), b("absent"), b("bb"))
			if len(got) != 3 || string(got[0]) != "1" || got[1] != nil || string(got[2]) != "2" {
				t.Fatalf("MGet=%q", got)
			}

			if err := c.FlushAll(bg); err != nil {
				t.Fatal(err)
			}
			if n, _ := c.DBSize(bg); n != 0 {
				t.Fatalf("DBSize after flush=%d", n)
			}
		})
	}
}

func TestCollections(t *testing.T) {
	for _, bk := range bothBackends(t) {
		t.Run(bk.name, func(t *testing.T) {
			url, cleanup := bk.url(t)
			defer cleanup()
			c := mustConnect(t, url)

			// Hash
			if n, _ := c.HSet(bg, b("h"), b("f1"), b("v1"), b("f2"), b("v2")); n != 2 {
				t.Fatalf("HSet newly-added=%d", n)
			}
			if n, _ := c.HSet(bg, b("h"), b("f1"), b("v1b")); n != 0 {
				t.Fatalf("HSet overwrite counted=%d", n)
			}
			if v, ok, _ := c.HGet(bg, b("h"), b("f1")); !ok || string(v) != "v1b" {
				t.Fatalf("HGet=%q %v", v, ok)
			}
			if n, _ := c.HLen(bg, b("h")); n != 2 {
				t.Fatalf("HLen=%d", n)
			}
			if all, _ := c.HGetAll(bg, b("h")); len(all) != 4 {
				t.Fatalf("HGetAll flat len=%d", len(all))
			}
			if ks, _ := c.HKeys(bg, b("h")); len(ks) != 2 {
				t.Fatalf("HKeys=%d", len(ks))
			}
			if vs, _ := c.HVals(bg, b("h")); len(vs) != 2 {
				t.Fatalf("HVals=%d", len(vs))
			}
			if n, _ := c.HDel(bg, b("h"), b("f1")); n != 1 {
				t.Fatalf("HDel=%d", n)
			}

			// List
			if n, _ := c.RPush(bg, b("l"), b("a"), b("b"), b("c")); n != 3 {
				t.Fatalf("RPush len=%d", n)
			}
			if n, _ := c.LPush(bg, b("l"), b("z")); n != 4 {
				t.Fatalf("LPush len=%d", n)
			}
			if n, _ := c.LLen(bg, b("l")); n != 4 {
				t.Fatalf("LLen=%d", n)
			}
			if r, _ := c.LRange(bg, b("l"), 0, -1); len(r) != 4 || string(r[0]) != "z" || string(r[3]) != "c" {
				t.Fatalf("LRange=%q", r)
			}
			if r, _ := c.LPop(bg, b("l"), 1); len(r) != 1 || string(r[0]) != "z" {
				t.Fatalf("LPop=%q", r)
			}
			if r, _ := c.RPop(bg, b("l"), 2); len(r) != 2 || string(r[0]) != "c" {
				t.Fatalf("RPop=%q", r)
			}

			// Set
			if n, _ := c.SAdd(bg, b("s1"), b("a"), b("b"), b("c")); n != 3 {
				t.Fatalf("SAdd=%d", n)
			}
			_, _ = c.SAdd(bg, b("s2"), b("b"), b("c"), b("d"))
			if n, _ := c.SCard(bg, b("s1")); n != 3 {
				t.Fatalf("SCard=%d", n)
			}
			if ok, _ := c.SIsMember(bg, b("s1"), b("a")); !ok {
				t.Fatal("SIsMember false")
			}
			if m, _ := c.SMembers(bg, b("s1")); len(m) != 3 {
				t.Fatalf("SMembers=%d", len(m))
			}
			if inter, _ := c.SInter(bg, b("s1"), b("s2")); len(inter) != 2 {
				t.Fatalf("SInter=%q", inter)
			}
			if uni, _ := c.SUnion(bg, b("s1"), b("s2")); len(uni) != 4 {
				t.Fatalf("SUnion=%q", uni)
			}
			if diff, _ := c.SDiff(bg, b("s1"), b("s2")); len(diff) != 1 || string(diff[0]) != "a" {
				t.Fatalf("SDiff=%q", diff)
			}
			if n, _ := c.SRem(bg, b("s1"), b("a")); n != 1 {
				t.Fatalf("SRem=%d", n)
			}

			// Sorted set
			if n, _ := c.ZAdd(bg, b("z"), ZMember{2, b("hi")}, ZMember{1, b("lo")}); n != 2 {
				t.Fatalf("ZAdd=%d", n)
			}
			if sc, ok, _ := c.ZScore(bg, b("z"), b("hi")); !ok || sc != 2 {
				t.Fatalf("ZScore=%v %v", sc, ok)
			}
			if n, _ := c.ZCard(bg, b("z")); n != 2 {
				t.Fatalf("ZCard=%d", n)
			}
			if r, _ := c.ZRange(bg, b("z"), 0, -1); len(r) != 2 || string(r[0]) != "lo" {
				t.Fatalf("ZRange asc=%q", r)
			}
			if n, _ := c.ZRem(bg, b("z"), b("lo")); n != 1 {
				t.Fatalf("ZRem=%d", n)
			}
		})
	}
}

func TestZAlgebra(t *testing.T) {
	for _, bk := range bothBackends(t) {
		t.Run(bk.name, func(t *testing.T) {
			url, cleanup := bk.url(t)
			defer cleanup()
			c := mustConnect(t, url)
			_, _ = c.ZAdd(bg, b("za"), ZMember{1, b("x")}, ZMember{2, b("y")})
			_, _ = c.ZAdd(bg, b("zb"), ZMember{10, b("y")}, ZMember{20, b("z")})

			if n, _ := c.ZInterStore(bg, b("zi"), b("za"), b("zb")); n != 1 {
				t.Fatalf("ZInterStore card=%d", n)
			}
			if sc, _, _ := c.ZScore(bg, b("zi"), b("y")); sc != 12 {
				t.Fatalf("ZInterStore SUM y=%v", sc)
			}
			if n, _ := c.ZUnionStore(bg, b("zu"), b("za"), b("zb")); n != 3 {
				t.Fatalf("ZUnionStore card=%d", n)
			}
			n, _ := c.ZUnionStoreWith(bg, b("zw"), [][]byte{b("za"), b("zb")}, []float64{2, 1}, AggMax)
			if n != 3 {
				t.Fatalf("ZUnionStoreWith card=%d", n)
			}
			if sc, _, _ := c.ZScore(bg, b("zw"), b("y")); sc != 10 {
				t.Fatalf("weighted MAX y=%v (want 10)", sc)
			}

			_, _ = c.ZAdd(bg, b("ia"), ZMember{1, b("a")}, ZMember{2, b("bb")}, ZMember{3, b("c")})
			_, _ = c.ZAdd(bg, b("ib"), ZMember{1, b("a")}, ZMember{2, b("bb")}, ZMember{9, b("q")})
			if n, _ := c.ZInterCard(bg, [][]byte{b("ia"), b("ib")}, -1); n != 2 {
				t.Fatalf("ZInterCard=%d", n)
			}
			if n, _ := c.ZInterCard(bg, [][]byte{b("ia"), b("ib")}, 1); n != 1 {
				t.Fatalf("ZInterCard LIMIT 1=%d", n)
			}
			if _, err := c.ZInterCard(bg, nil, -1); !IsKind(err, KindInvalidInput) {
				t.Fatalf("empty keys should be InvalidInput, got %v", err)
			}
		})
	}
}

func TestHashFieldTTL(t *testing.T) {
	for _, bk := range bothBackends(t) {
		t.Run(bk.name, func(t *testing.T) {
			url, cleanup := bk.url(t)
			defer cleanup()
			c := mustConnect(t, url)
			_, _ = c.HSet(bg, b("h"), b("a"), b("1"), b("bb"), b("2"))

			codes, err := c.HExpire(bg, b("h"), [][]byte{b("a"), b("nope")}, 60*time.Second, CondAlways)
			if err != nil {
				t.Fatal(err)
			}
			if len(codes) != 2 || codes[0] != 1 || codes[1] != -2 {
				t.Fatalf("HExpire codes=%v (want [1 -2])", codes)
			}
			ttls, _ := c.HTTL(bg, b("h"), b("a"), b("bb"))
			if len(ttls) != 2 || ttls[0] <= 0 || ttls[0] > 60 || ttls[1] != -1 {
				t.Fatalf("HTTL secs=%v", ttls)
			}
			codes, _ = c.HPersist(bg, b("h"), b("a"), b("bb"))
			if len(codes) != 2 || codes[0] != 1 || codes[1] != -1 {
				t.Fatalf("HPersist codes=%v (want [1 -1])", codes)
			}
			// hpexpire ms precision.
			pc, _ := c.HPExpire(bg, b("h"), [][]byte{b("a")}, 1500*time.Millisecond, CondAlways)
			if len(pc) != 1 || pc[0] != 1 {
				t.Fatalf("HPExpire=%v", pc)
			}
			pt, _ := c.HPTTL(bg, b("h"), b("a"))
			if len(pt) != 1 || pt[0] <= 0 || pt[0] > 1500 {
				t.Fatalf("HPTTL ms=%v", pt)
			}
			if _, err := c.HTTL(bg, b("h")); !IsKind(err, KindInvalidInput) {
				t.Fatalf("empty fields should be InvalidInput, got %v", err)
			}
		})
	}
}

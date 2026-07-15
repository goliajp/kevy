package kevy

import (
	"testing"
)

// Cluster client CRC16 routing (contract §3.15, §6). Requires the server
// in cluster mode; each shard binds port+1+i and answers a wrong-shard
// key with -MOVED, so correct routing means -MOVED never fires.

func TestClusterRouting(t *testing.T) {
	// --cluster with 4 shards: main port P, shard ports P+1..P+4.
	s := spawnServer(t, "--cluster", "--threads", "4")
	cc, err := ClusterConnect("127.0.0.1", uint16(s.port))
	if err != nil {
		t.Fatalf("ClusterConnect: %v", err)
	}
	defer cc.Close()

	if cc.ShardCount() != 4 {
		t.Fatalf("ShardCount=%d want 4", cc.ShardCount())
	}

	// Keys spanning slots: each routes to its owner shard with no -MOVED.
	keys := []string{"k0", "k1", "user:42", "rate:10.0.0.1", "gl:abc", "alpha", "beta", "gamma"}
	for i, k := range keys {
		val := b("v" + itoa(i))
		if err := cc.Set(bg, b(k), val); err != nil {
			t.Fatalf("Set %q routed wrong (-MOVED?): %v", k, err)
		}
		got, ok, gerr := cc.Get(bg, b(k))
		if gerr != nil || !ok || string(got) != string(val) {
			t.Fatalf("Get %q = %q %v %v", k, got, ok, gerr)
		}
	}

	if _, err := cc.Incr(bg, b("counter")); err != nil {
		t.Fatal(err)
	}
	if err := cc.Ping(bg); err != nil {
		t.Fatal(err)
	}

	// del/exists route per key and sum across shards.
	n, err := cc.Del(bg, b("k0"), b("k1"), b("user:42"), b("absent"))
	if err != nil {
		t.Fatal(err)
	}
	if n != 3 {
		t.Fatalf("cluster Del cross-shard sum=%d want 3", n)
	}
	cnt, err := cc.Exists(bg, b("alpha"), b("beta"), b("gamma"))
	if err != nil {
		t.Fatal(err)
	}
	if cnt != 3 {
		t.Fatalf("cluster Exists sum=%d want 3", cnt)
	}

	// dbsize is whole-cluster (server fans out internally).
	if sz, err := cc.DBSize(bg); err != nil || sz < 1 {
		t.Fatalf("cluster DBSize=%d %v", sz, err)
	}
	if err := cc.FlushAll(bg); err != nil {
		t.Fatal(err)
	}
}

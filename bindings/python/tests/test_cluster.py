"""Cluster client CRC16 routing (contract §3.15, §6). Remote-only.

The server binds one extra deterministic port per shard; a wrong-shard key
answers -MOVED, so correct routing means -MOVED never fires."""

import kevy
import pytest
from conftest import spawn_server


@pytest.fixture(scope="module")
def cluster_server():
    srv = spawn_server("--cluster", "--threads", "4")
    yield srv
    srv.stop()


def test_cluster_routing(cluster_server):
    cc = kevy.ClusterClient.connect("127.0.0.1", cluster_server.port)
    assert cc.shard_count == 4

    keys = ["k0", "k1", "user:42", "rate:10.0.0.1", "gl:abc", "alpha", "beta", "gamma"]
    for i, k in enumerate(keys):
        val = f"v{i}".encode()
        cc.set(k, val)  # routed with no -MOVED, else this errors
        assert cc.get(k) == val

    cc.incr("counter")
    cc.ping()

    # del/exists route per key and sum across shards.
    assert cc.delete("k0", "k1", "user:42", "absent") == 3
    assert cc.exists("alpha", "beta", "gamma") == 3

    # dbsize is whole-cluster (server fans out internally).
    assert cc.dbsize() >= 1
    cc.flushall()
    cc.close()


def test_hashtag_same_slot_combine(cluster_server):
    cc = kevy.ClusterClient.connect("127.0.0.1", cluster_server.port)
    # {tag} forces same-slot so a set-combine is answerable on one shard.
    cc.request_keyed("{g}:a", b"SADD", b"{g}:a", b"1", b"2")
    cc.request_keyed("{g}:b", b"SADD", b"{g}:b", b"2", b"3")
    r = cc.request_keyed("{g}:a", b"SINTER", b"{g}:a", b"{g}:b")
    assert {it.data for it in r.items} == {b"2"}
    cc.close()

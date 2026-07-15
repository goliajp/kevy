"""kevy-embedded store contract over the C ABI (contract §5, §6)."""

import kevy
import pytest


def test_open_mem_cmd_scalar():
    db = kevy.open_mem()
    assert db.cmd(b"SET", b"k", b"v").data == b"OK"
    assert db.cmd(b"GET", b"k").data == b"v"
    # scalar fast paths
    db.set(b"k2", b"hello")
    assert db.get(b"k2") == b"hello"
    assert db.get(b"missing") is None
    db.set(b"k3", b"tmp", ttl_ms=100_000)
    assert db.get(b"k3") == b"tmp"
    db.close()
    db.close()  # idempotent-safe


def test_cmd_arbitrary_verb_parseable():
    db = kevy.open_mem()
    db.cmd(b"RPUSH", b"l", b"a", b"b", b"c")
    r = db.cmd(b"LRANGE", b"l", b"0", b"-1")
    assert [it.data for it in r.items] == [b"a", b"b", b"c"]
    db.close()


def test_meta():
    assert kevy.abi() == 1
    assert kevy.version()  # non-empty engine version string


def test_embedded_subscribe_poll_and_wait():
    # A named bus so a publisher on the same URL reaches the subscription.
    name = "mem://emb-sub-bus"
    pub = kevy.connect(name)
    db, _ = _shared_db(name)
    sub = db.subscribe(b"chan")
    # The subscribe ack (*3 subscribe chan :1) is delivered first (§5.1).
    ack = sub.next()
    assert ack is not None and ack.items[0].data == b"subscribe"
    # nothing else queued yet
    assert sub.next() is None
    assert pub.publish("chan", "hello") >= 1
    frame = sub.wait(2000)  # block up to 2s
    assert frame is not None
    # *3 message chan hello
    assert frame.items[0].data == b"message"
    assert frame.items[2].data == b"hello"
    sub.close()
    pub.close()


def _shared_db(url: str):
    # Resolve the same shared store the client URL uses.
    from kevy._url import parse_connect_url, resolve_store

    return resolve_store(parse_connect_url(url))


def test_persistence_survives_reopen(tmp_path):
    directory = str(tmp_path / "store")
    db = kevy.open_persistent(directory)
    db.cmd(b"SET", b"survivor", b"yes")
    db.close()
    db2 = kevy.open_persistent(directory)
    assert db2.cmd(b"GET", b"survivor").data == b"yes"  # snapshot + AOF replay
    db2.close()

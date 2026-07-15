"""Core KV round-trips (contract §3.1, §6) on both backends."""

import time

import kevy


def test_set_get_del_exists(backend):
    c = backend.connect()
    c.set("k", "v")
    assert c.get("k") == b"v"
    assert c.get(b"missing") is None
    assert c.exists("k", "k", "missing") == 2  # repeated key counts each time
    assert c.delete("k") == 1
    assert c.get("k") is None
    c.close()


def test_incr_family(backend):
    c = backend.connect()
    assert c.incr("n") == 1
    assert c.incr("n") == 2
    assert c.incr_by("n", 10) == 12
    assert c.incr_by("n", -5) == 7
    c.close()


def test_binary_safe_values(backend):
    c = backend.connect()
    key = bytes([0, 1, 2, 255])
    val = b"a\r\nb\x00c"
    c.set(key, val)
    assert c.get(key) == val
    c.close()


def test_expire_persist_ttl(backend):
    c = backend.connect()
    c.set("k", "v")
    assert c.ttl_ms("k") == -1  # no TTL
    assert c.ttl_ms("nope") == -2  # no key
    assert c.expire("k", 100.0) is True
    assert c.ttl_ms("k") > 0
    assert c.persist("k") is True
    assert c.ttl_ms("k") == -1
    c.close()


def test_set_with_ttl_atomic(backend):
    c = backend.connect()
    c.set_with_ttl("k", "v", 100.0)
    assert c.get("k") == b"v"
    assert c.ttl_ms("k") > 0
    c.close()


def test_type_of_and_dbsize_flushall(backend):
    c = backend.connect()
    c.set("s", "v")
    c.rpush("l", "a")
    c.hset("h", "f", "v")
    c.sadd("st", "m")
    c.zadd("z", (1.0, "m"))
    assert c.type_of("s") == "string"
    assert c.type_of("l") == "list"
    assert c.type_of("h") == "hash"
    assert c.type_of("st") == "set"
    assert c.type_of("z") == "zset"
    assert c.type_of("nope") == "none"
    assert c.dbsize() >= 5
    c.flushall()
    assert c.dbsize() == 0
    c.close()


def test_mget_mset(backend):
    c = backend.connect()
    c.mset("a", "1", "b", "2")
    assert c.mget("a", "missing", "b") == [b"1", None, b"2"]
    c.close()


def test_mset_odd_arity_rejected(backend):
    c = backend.connect()
    try:
        c.mset("a", "1", "b")
        assert False, "expected InvalidInputError"
    except kevy.InvalidInputError:
        pass
    c.close()


def test_ping(backend):
    c = backend.connect()
    c.ping()  # no error
    c.close()

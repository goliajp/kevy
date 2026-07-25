"""Collections: hash / list / set / sorted set (contract §3.2–§3.5, §6)."""

import kevy


def test_hash(backend):
    c = backend.connect()
    assert c.hset("h", "f1", "v1", "f2", "v2") == 2  # newly-added count
    assert c.hset("h", "f1", "V1") == 0  # overwrite, not new
    assert c.hget("h", "f1") == b"V1"
    assert c.hget("h", "nope") is None
    assert c.hlen("h") == 2
    flat = c.hgetall("h")
    assert len(flat) == 4  # flat [f0,v0,f1,v1]
    assert set(c.hkeys("h")) == {b"f1", b"f2"}
    assert set(c.hvals("h")) == {b"V1", b"v2"}
    assert c.hdel("h", "f1") == 1
    assert c.hlen("h") == 1
    c.close()


def test_list(backend):
    c = backend.connect()
    assert c.lpush("l", "b", "a") == 2  # new length; a then b at head
    assert c.rpush("l", "c") == 3
    assert c.llen("l") == 3
    assert c.lrange("l", 0, -1) == [b"a", b"b", b"c"]
    assert c.lrange("l", -2, -1) == [b"b", b"c"]  # negative indexing
    assert c.lpop("l", 1) == [b"a"]
    assert c.rpop("l", 1) == [b"c"]
    assert c.lpop("drained", 2) == []
    c.close()


def test_set(backend):
    c = backend.connect()
    assert c.sadd("s", "x", "y", "x") == 2  # newly-added dedup
    assert c.scard("s") == 2
    assert c.sismember("s", "x") is True
    assert c.sismember("s", "z") is False
    assert set(c.smembers("s")) == {b"x", b"y"}
    assert c.srem("s", "x") == 1
    c.close()


def test_set_combines(backend):
    c = backend.connect()
    c.sadd("a", "1", "2", "3")
    c.sadd("b", "2", "3", "4")
    assert set(c.sinter("a", "b")) == {b"2", b"3"}
    assert set(c.sunion("a", "b")) == {b"1", b"2", b"3", b"4"}
    assert set(c.sdiff("a", "b")) == {b"1"}
    c.close()


def test_zset(backend):
    c = backend.connect()
    assert c.zadd("z", (2.0, "b"), (1.0, "a"), (3.0, "c")) == 3
    assert c.zadd("z", kevy.ZMember(1.5, "a")) == 0  # update, not new
    assert c.zcard("z") == 3
    assert c.zscore("z", "a") == 1.5
    assert c.zscore("z", "nope") is None
    assert c.zrange("z", 0, -1) == [b"a", b"b", b"c"]  # ascending score
    assert c.zrem("z", "b") == 1
    assert c.zrange("z", 0, -1) == [b"a", b"c"]
    c.close()

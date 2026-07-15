"""The asyncio face (contract §1.4, §6): one package, both faces, agreeing
results — plus async coverage of the family set."""

import kevy
import pytest


async def test_async_core_roundtrip(backend):
    ac = await backend.aconnect()
    await ac.set("k", "v")
    assert await ac.get("k") == b"v"
    assert await ac.get(b"missing") is None
    assert await ac.incr("n") == 1
    assert await ac.incr_by("n", 4) == 5
    await ac.close()


async def test_sync_and_async_agree(backend):
    # Both faces run against the same backing store/server and agree.
    sync_c = backend.connect()
    async_c = await backend.aconnect()
    sync_c.set("shared", "written-by-sync")
    assert await async_c.get("shared") == b"written-by-sync"
    await async_c.set("shared2", "written-by-async")
    assert sync_c.get("shared2") == b"written-by-async"
    sync_c.close()
    await async_c.close()


async def test_async_collections(backend):
    ac = await backend.aconnect()
    assert await ac.hset("h", "f", "v") == 1
    assert await ac.hget("h", "f") == b"v"
    assert await ac.rpush("l", "a", "b") == 2
    assert await ac.lrange("l", 0, -1) == [b"a", b"b"]
    assert await ac.sadd("s", "x", "y") == 2
    assert await ac.zadd("z", (1.0, "a")) == 1
    assert await ac.zscore("z", "a") == 1.0
    await ac.close()


async def test_async_zalgebra_and_hashttl(backend):
    ac = await backend.aconnect()
    await ac.zadd("a", (1.0, "x"), (2.0, "y"))
    await ac.zadd("b", (5.0, "y"))
    assert await ac.zinterstore("d", "a", "b") == 1
    await ac.hset("h", "f", "v")
    codes = await ac.hexpire("h", ["f"], 100.0)
    assert codes[0] == 1
    await ac.close()


async def test_async_blocking_pop(backend):
    ac = await backend.aconnect()
    await ac.rpush("q", "a")
    assert await ac.blpop(["q"], timeout=1.0) == (b"q", b"a")
    assert await ac.blpop(["empty"], timeout=0.2) is None
    await ac.close()


async def test_async_pubsub(backend):
    url = backend.url
    sub = await kevy.AsyncSubscriber.connect(url)
    await sub.subscribe("ch")
    ack = await sub.recv()
    assert ack.kind is kevy.PubsubKind.SUBSCRIBE
    pub = await kevy.AsyncClient.connect(url)
    await pub.publish("ch", "hi")
    ch, payload = await sub.recv_message()
    assert ch == b"ch" and payload == b"hi"
    await pub.close()
    await sub.close()


async def test_async_transaction(remote_server):
    ac = await kevy.AsyncClient.connect(remote_server.url)
    tx = await ac.multi()
    await tx.set("a", "1")
    await tx.incr("n")
    replies = await tx.exec()
    assert replies[0].data == b"OK" and replies[1].integer == 1
    await ac.close()


async def test_async_pipeline(remote_server):
    ac = await kevy.AsyncClient.connect(remote_server.url)
    replies = await ac.pipeline(lambda p: p.cmd(b"SET", b"k", b"v").cmd(b"GET", b"k"))
    assert replies[1].data == b"v"
    await ac.close()


async def test_async_embedded_unsupported():
    ac = await kevy.AsyncClient.connect("mem://async-unsup")
    with pytest.raises(kevy.UnsupportedError):
        await ac.idx_list()
    await ac.close()

"""Blocking pops: BLPOP / BRPOP / BZPOPMIN (contract §3.14, §6).

On the embedded C-ABI backend these are emulated by polling; on remote the
server parks the connection. Both honour the same timeout contract."""

import threading
import time

import kevy
import pytest


def test_blpop_immediate_hit(backend):
    c = backend.connect()
    c.rpush("q", "a", "b")
    assert c.blpop(["q"], timeout=1.0) == (b"q", b"a")  # head
    assert c.brpop(["q"], timeout=1.0) == (b"q", b"b")  # tail
    c.close()


def test_blpop_timeout_miss(backend):
    c = backend.connect()
    start = time.monotonic()
    assert c.blpop(["empty"], timeout=0.2) is None
    assert time.monotonic() - start >= 0.15
    c.close()


def test_bzpopmin_lowest_score(backend):
    c = backend.connect()
    c.zadd("z", (3.0, "c"), (1.0, "a"), (2.0, "b"))
    hit = c.bzpopmin(["z"], timeout=1.0)
    assert hit == (b"z", b"a", 1.0)
    c.close()


def test_some_zero_timeout_invalid(backend):
    c = backend.connect()
    with pytest.raises(kevy.InvalidInputError):
        c.blpop(["q"], timeout=0)
    c.close()


def test_empty_keys_invalid(backend):
    c = backend.connect()
    with pytest.raises(kevy.InvalidInputError):
        c.blpop([], timeout=1.0)
    c.close()


def test_blpop_wakes_on_concurrent_push(backend):
    # A blocked pop wakes when another connection pushes (observably correct;
    # embedded differs only in a bounded poll latency).
    url = backend.url
    consumer = kevy.connect(url)
    producer = kevy.connect(url)

    def push_later():
        time.sleep(0.2)
        producer.rpush("wq", "delivered")

    t = threading.Thread(target=push_later)
    t.start()
    hit = consumer.blpop(["wq"], timeout=3.0)
    t.join()
    assert hit == (b"wq", b"delivered")
    consumer.close()
    producer.close()

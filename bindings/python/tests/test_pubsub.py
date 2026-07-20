"""Pub/sub round-trip (contract §3.11, §6) on both backends."""

import kevy
import pytest
from kevy import PubsubKind


def test_publish_subscribe_message(backend):
    url = backend.url
    sub = kevy.Subscriber.connect(url)
    sub.subscribe("news")
    ack = sub.recv()  # drain the subscribe ack (also confirms active)
    assert ack.kind is PubsubKind.SUBSCRIBE and ack.channel == b"news"

    pub = kevy.connect(url)
    pub.publish("news", "hello")
    ch, payload = sub.recv_message()  # ack frames are skipped by recv_message
    assert ch == b"news" and payload == b"hello"

    pub.close()
    sub.close()


def test_pattern_subscribe_pmessage(backend):
    url = backend.url
    sub = kevy.Subscriber.connect(url)
    sub.psubscribe("news.*")
    ack = sub.recv()
    assert ack.kind is PubsubKind.PSUBSCRIBE

    pub = kevy.connect(url)
    pub.publish("news.sports", "goal")
    ev = sub.recv()
    assert ev.kind is PubsubKind.PMESSAGE
    assert ev.pattern == b"news.*" and ev.channel == b"news.sports" and ev.payload == b"goal"

    pub.close()
    sub.close()


def test_connect_channels_and_recv_message(backend):
    url = backend.url
    sub = kevy.Subscriber.connect_channels(url, "a", "b")
    # Drain both subscribe acks BEFORE publishing. `subscribe()` writes the
    # SUBSCRIBE and returns — it does not wait for the server to register
    # it (see `test_read_timeout_bounds_recv`, which reads the ack
    # explicitly). Publishing first is a race: the message goes to whoever
    # is registered at PUBLISH time, and losing it parks `recv_message()`
    # forever. That is exactly how the python conformance job hung for
    # 3h46m in CI; it reproduces about 1 run in 10 on a real Linux box.
    for _ in range(2):
        sub.recv()
    pub = kevy.connect(url)
    pub.publish("b", "x")
    ch, payload = sub.recv_message()
    assert ch == b"b" and payload == b"x"
    pub.close()
    sub.close()


def test_anonymous_mem_rejected():
    with pytest.raises(kevy.UnsupportedError):
        kevy.Subscriber.connect("mem://")


def test_read_timeout_bounds_recv():
    # A named bus with no publisher: recv times out.
    sub = kevy.Subscriber.connect_channels("mem://timeout-bus", "c")
    sub.recv()  # subscribe ack
    sub.set_read_timeout(0.2)
    with pytest.raises(kevy.TimedOutError):
        sub.recv()
    sub.close()


def test_connect_channels_empty_invalid():
    with pytest.raises(kevy.InvalidInputError):
        kevy.Subscriber.connect_channels("mem://x")

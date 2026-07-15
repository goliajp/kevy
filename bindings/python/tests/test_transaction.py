"""Transactions: MULTI / EXEC / DISCARD + WATCH (contract §3.12, §6).
Remote-only."""

import kevy
import pytest
from kevy import ReplyKind


def test_multi_queue_exec_order(remote_server):
    c = kevy.connect(remote_server.url)
    tx = c.multi()
    tx.set("a", "1").incr("n").get("a")
    replies = tx.exec()
    assert len(replies) == 3
    assert replies[0].kind is ReplyKind.SIMPLE and replies[0].data == b"OK"
    assert replies[1].integer == 1
    assert replies[2].data == b"1"
    c.close()


def test_exec_typed_cursor(remote_server):
    c = kevy.connect(remote_server.url)
    c.set("pre", "p")  # committed before the block (see deviation note below)
    tx = c.multi()
    tx.set("a", "1").incr("n").mget("pre", "missing")
    cur = tx.exec_typed()
    cur.next_ok()
    assert cur.next_int() == 1
    assert cur.next_array_of_bulks() == [b"p", None]
    cur.expect_empty()  # arity gate
    c.close()


@pytest.mark.xfail(
    strict=True,
    reason=(
        "kevy SERVER deviation (client is correct): MGET queued in a MULTI does "
        "not observe writes made earlier in the SAME MULTI block, while GET does. "
        "The Go reference port never exercised MGET-in-MULTI, so it did not surface "
        "this. See client-contract.md §3.12 (EXEC = array of N typed replies, one "
        "per queued command, executed in order). If this test starts passing the "
        "server was fixed — drop the xfail."
    ),
)
def test_mget_in_multi_sees_earlier_write_deviation(remote_server):
    c = kevy.connect(remote_server.url)
    tx = c.multi()
    tx.set("w", "1").mget("w")
    replies = tx.exec()
    # GET would see it; MGET should too — but the server returns [Nil].
    assert replies[1].items[0].data == b"1"
    c.close()


def test_watch_abort(remote_server):
    c = kevy.connect(remote_server.url)
    other = kevy.connect(remote_server.url)
    c.watch("wk")
    other.set("wk", "changed")  # concurrent modify after WATCH
    tx = c.multi()
    tx.set("wk", "mine")
    assert tx.exec_watched() is None  # WATCH violation → abort
    assert other.get("wk") == b"changed"
    c.close()
    other.close()


def test_abandon_sends_implicit_discard(remote_server):
    c = kevy.connect(remote_server.url)
    with c.multi() as tx:
        tx.set("x", "1")
        # leave the block without exec/discard — __exit__ sends DISCARD
    # socket is not stuck in MULTI mode: a normal command works
    c.set("y", "ok")
    assert c.get("y") == b"ok"
    assert c.get("x") is None  # the queued SET was discarded
    c.close()


def test_queue_typed_builders(remote_server):
    c = kevy.connect(remote_server.url)
    tx = c.multi()
    tx.queue(b"SET", b"k", b"v")
    replies = tx.exec()
    assert replies[0].data == b"OK"
    c.close()

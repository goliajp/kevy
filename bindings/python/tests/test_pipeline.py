"""Pipeline (non-atomic batching, contract §3.13, §6). Remote-only."""

import kevy
from kevy import ReplyKind


def test_pipeline_order_and_inline_errors(remote_server):
    c = kevy.connect(remote_server.url)
    replies = c.pipeline(
        lambda p: p.cmd(b"SET", b"k", b"v")
        .cmd(b"GET", b"k")
        .cmd(b"INCR", b"k")  # k is not an integer → inline error, batch not aborted
        .cmd(b"GET", b"k")
    )
    assert len(replies) == 4
    assert replies[0].data == b"OK"
    assert replies[1].data == b"v"
    assert replies[2].is_error()  # per-command error, inline (did not abort)
    assert replies[3].data == b"v"
    c.close()


def test_empty_batch_no_io(remote_server):
    c = kevy.connect(remote_server.url)
    assert c.pipeline(lambda p: None) == []
    c.close()


def test_empty_argv_poisons(remote_server):
    c = kevy.connect(remote_server.url)
    try:
        c.pipeline(lambda p: p.cmd(b"PING").cmd())  # empty argv
        assert False, "expected InvalidInputError"
    except kevy.InvalidInputError:
        pass
    c.close()


def test_pipeline_len(remote_server):
    c = kevy.connect(remote_server.url)

    def build(p):
        p.cmd(b"PING")
        p.cmd(b"PING")
        assert len(p) == 2 and not p.is_empty()

    c.pipeline(build)
    c.close()


def test_pipeline_embedded_unsupported():
    c = kevy.connect("mem://pipe")
    try:
        c.pipeline(lambda p: p.cmd(b"PING"))
        assert False
    except kevy.UnsupportedError:
        pass
    c.close()

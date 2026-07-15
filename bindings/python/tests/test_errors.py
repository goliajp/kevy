"""Error-as-exception mapping (contract §2, §6) on both backends."""

import kevy
import pytest


def test_wrongtype_surfaces_store_error(backend):
    c = backend.connect()
    c.rpush("l", "a")  # l is a list
    with pytest.raises(kevy.WrongTypeError) as ei:
        c.get("l")  # GET on a list → WRONGTYPE
    assert isinstance(ei.value, kevy.StoreError)
    assert ei.value.kind is kevy.StoreErrorKind.WRONG_TYPE
    c.close()


def test_incr_non_integer(backend):
    c = backend.connect()
    c.set("k", "notanumber")
    with pytest.raises(kevy.NotIntegerError) as ei:
        c.incr("k")
    assert ei.value.kind is kevy.StoreErrorKind.NOT_INTEGER
    c.close()


def test_protocol_error_preserves_wire_text():
    # A non-store -ERR maps to ProtocolError with the wire text verbatim
    # (§2.2). This is the exact classifier the typed methods raise through.
    from kevy._errors import error_from_reply_text

    err = error_from_reply_text(b"ERR wrong number of arguments for 'set'")
    assert isinstance(err, kevy.ProtocolError)
    assert not isinstance(err, kevy.StoreError)
    assert str(err) == "ERR wrong number of arguments for 'set'"


def test_remote_typed_method_raises_protocol(remote_server):
    # A typed remote method surfaces a server -ERR as a raised error.
    c = kevy.connect(remote_server.url)
    with pytest.raises(kevy.KevyError):
        c.idx_query_raw(b"no-such-index", b"RANGE", b"0", b"100")
    c.close()


def test_embedded_only_unsupported():
    c = kevy.connect("mem://unsup")
    with pytest.raises(kevy.UnsupportedError):
        c.idx_list()
    with pytest.raises(kevy.UnsupportedError):
        c.multi()
    with pytest.raises(kevy.UnsupportedError):
        c.pipeline(lambda p: p.cmd(b"PING"))
    c.close()


def test_do_raw_returns_error_reply_as_data(backend):
    c = backend.connect()
    r = c.do(b"INCR")  # arity error, but do() returns the raw Reply
    assert r.is_error()  # data, not a raised exception
    c.close()


def test_closed_after_close(backend):
    c = backend.connect()
    c.close()
    with pytest.raises(kevy.KevyError):
        c.get("k")

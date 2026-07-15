"""RESP2 + RESP3 reply parser (contract §4.1)."""

import kevy
import pytest
from kevy._reply import ReplyKind, decode_reply, parse_reply


def test_simple_and_error_and_int():
    assert decode_reply(b"+OK\r\n").kind is ReplyKind.SIMPLE
    assert decode_reply(b"+OK\r\n").data == b"OK"
    e = decode_reply(b"-ERR nope\r\n")
    assert e.kind is ReplyKind.ERROR and e.data == b"ERR nope"
    assert decode_reply(b":42\r\n").integer == 42


def test_bulk_and_nil():
    assert decode_reply(b"$3\r\nabc\r\n").data == b"abc"
    assert decode_reply(b"$-1\r\n").kind is ReplyKind.NIL
    assert decode_reply(b"$0\r\n\r\n").data == b""


def test_binary_safe_bulk_with_crlf():
    r = decode_reply(b"$4\r\na\r\nb\r\n")  # payload literally contains CRLF
    assert r.data == b"a\r\nb"


def test_array_nested():
    r = decode_reply(b"*2\r\n:1\r\n$2\r\nhi\r\n")
    assert r.kind is ReplyKind.ARRAY
    assert r.items[0].integer == 1
    assert r.items[1].data == b"hi"


def test_resp3_shapes():
    assert decode_reply(b",3.14\r\n").double == pytest.approx(3.14)
    assert decode_reply(b"#t\r\n").boolean is True
    assert decode_reply(b"_\r\n").kind is ReplyKind.NULL
    m = decode_reply(b"%1\r\n$1\r\nk\r\n:9\r\n")
    assert m.kind is ReplyKind.MAP and m.pairs[0][1].integer == 9
    v = decode_reply(b"=9\r\ntxt:hello\r\n")
    assert v.kind is ReplyKind.VERBATIM and v.data == b"hello" and v.verbatim_fmt == b"txt"


def test_push_frame():
    r = decode_reply(b">3\r\n$7\r\nmessage\r\n$2\r\nch\r\n$2\r\nhi\r\n")
    assert r.kind is ReplyKind.PUSH and r.items[0].data == b"message"


def test_attribute_frame_skipped():
    # |1 ... attribute then the real reply
    r = decode_reply(b"|1\r\n$3\r\nttl\r\n:10\r\n:7\r\n")
    assert r.kind is ReplyKind.INT and r.integer == 7


def test_need_more_bytes():
    reply, used = parse_reply(b"$3\r\nab")  # incomplete
    assert reply is None and used == 0


def test_malformed_tag():
    with pytest.raises(kevy.ProtocolError):
        decode_reply(b"?bogus\r\n")

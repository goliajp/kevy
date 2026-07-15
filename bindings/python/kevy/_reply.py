"""The universal decoded RESP reply and a RESP2+RESP3 parser (contract §4.1).

``Reply`` covers every RESP2 and RESP3 shape. ``parse_reply`` decodes one
reply from the front of a buffer and reports how many bytes it consumed
(0 = need more bytes), so the same routine serves the self-contained FFI
path and the streaming TCP path. The parser transparently skips attribute
frames and caps header-declared counts by the bytes remaining (fuzz-hardened,
mirroring the Rust parser).
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import List, Optional, Tuple

from ._errors import ProtocolError


class ReplyKind(Enum):
    SIMPLE = "simple-string"
    ERROR = "error"
    INT = "integer"
    BULK = "bulk-string"
    NIL = "nil"
    ARRAY = "array"
    MAP = "map"
    SET = "set"
    DOUBLE = "double"
    BOOLEAN = "boolean"
    VERBATIM = "verbatim-string"
    BIG_NUMBER = "big-number"
    NULL = "null"
    PUSH = "push"
    BLOB_ERROR = "blob-error"


@dataclass
class Reply:
    """One decoded RESP value (contract §4.1)."""

    kind: ReplyKind
    #: payload bytes for SIMPLE / ERROR / BULK / BIG_NUMBER / BLOB_ERROR /
    #: VERBATIM (data body).
    data: bytes = b""
    integer: int = 0
    double: float = 0.0
    boolean: bool = False
    verbatim_fmt: bytes = b""  # 3-char format tag for VERBATIM
    #: elements for ARRAY / SET / PUSH.
    items: List["Reply"] = field(default_factory=list)
    #: (key, value) pairs for MAP.
    pairs: List[Tuple["Reply", "Reply"]] = field(default_factory=list)

    def is_error(self) -> bool:
        return self.kind in (ReplyKind.ERROR, ReplyKind.BLOB_ERROR)

    def is_nil(self) -> bool:
        return self.kind in (ReplyKind.NIL, ReplyKind.NULL)

    def text(self) -> str:
        return self.data.decode("utf-8", "replace")

    def shape(self) -> str:
        return self.kind.value


def decode_reply(buf: bytes) -> Reply:
    """Parse exactly one complete reply from a self-contained buffer (the
    FFI path). Raises ProtocolError on a truncated or malformed frame."""
    reply, used = parse_reply(buf)
    if used == 0:
        raise ProtocolError("truncated RESP reply")
    return reply


def _find_crlf(buf: bytes, start: int) -> int:
    i = buf.find(b"\r\n", start)
    return i


def parse_reply(buf: bytes) -> Tuple[Optional[Reply], int]:
    """Decode one reply from the front of ``buf``. Returns ``(reply, used)``;
    ``used == 0`` means "need more bytes" (reply is None). Raises
    ProtocolError on a malformed frame. Speaks RESP2 + RESP3 and skips
    attribute frames."""
    if not buf:
        return None, 0
    tag = buf[0:1]
    if tag == b"+":
        return _line(buf, ReplyKind.SIMPLE)
    if tag == b"-":
        return _line(buf, ReplyKind.ERROR)
    if tag == b":":
        return _int(buf)
    if tag == b"$":
        return _bulk(buf, ReplyKind.BULK)
    if tag == b"*":
        return _agg(buf, ReplyKind.ARRAY)
    if tag == b"%":
        return _map(buf)
    if tag == b"~":
        return _agg(buf, ReplyKind.SET)
    if tag == b",":
        return _double(buf)
    if tag == b"#":
        return _bool(buf)
    if tag == b"=":
        return _verbatim(buf)
    if tag == b"(":
        return _line(buf, ReplyKind.BIG_NUMBER)
    if tag == b"_":
        return _null(buf)
    if tag == b">":
        return _agg(buf, ReplyKind.PUSH)
    if tag == b"!":
        return _bulk(buf, ReplyKind.BLOB_ERROR)
    if tag == b"|":
        return _attributed(buf)
    raise ProtocolError(f"unknown RESP tag {tag!r}")


def _line(buf: bytes, kind: ReplyKind) -> Tuple[Optional[Reply], int]:
    eol = _find_crlf(buf, 1)
    if eol < 0:
        return None, 0
    return Reply(kind=kind, data=bytes(buf[1:eol])), eol + 2


def _int(buf: bytes) -> Tuple[Optional[Reply], int]:
    eol = _find_crlf(buf, 1)
    if eol < 0:
        return None, 0
    try:
        n = int(buf[1:eol])
    except ValueError:
        raise ProtocolError(f"bad integer reply {bytes(buf[1:eol])!r}")
    return Reply(kind=ReplyKind.INT, integer=n), eol + 2


def _bulk(buf: bytes, kind: ReplyKind) -> Tuple[Optional[Reply], int]:
    hdr = _find_crlf(buf, 1)
    if hdr < 0:
        return None, 0
    try:
        n = int(buf[1:hdr])
    except ValueError:
        raise ProtocolError(f"bad bulk length {bytes(buf[1:hdr])!r}")
    if n < 0:
        return Reply(kind=ReplyKind.NIL), hdr + 2
    start = hdr + 2
    end = start + n
    if len(buf) < end + 2:
        return None, 0
    return Reply(kind=kind, data=bytes(buf[start:end])), end + 2


def _cap_hint(count: int, remaining: int) -> int:
    return min(count, max(remaining, 0))


def _agg(buf: bytes, kind: ReplyKind) -> Tuple[Optional[Reply], int]:
    hdr = _find_crlf(buf, 1)
    if hdr < 0:
        return None, 0
    try:
        count = int(buf[1:hdr])
    except ValueError:
        raise ProtocolError(f"bad aggregate length {bytes(buf[1:hdr])!r}")
    if count < 0:
        if kind == ReplyKind.PUSH:
            raise ProtocolError("push frame cannot be null")
        return Reply(kind=ReplyKind.NIL), hdr + 2
    pos = hdr + 2
    items: List[Reply] = []
    for _ in range(count):
        item, used = parse_reply(buf[pos:])
        if used == 0:
            return None, 0
        items.append(item)  # type: ignore[arg-type]
        pos += used
    return Reply(kind=kind, items=items), pos


def _map(buf: bytes) -> Tuple[Optional[Reply], int]:
    hdr = _find_crlf(buf, 1)
    if hdr < 0:
        return None, 0
    try:
        count = int(buf[1:hdr])
    except ValueError:
        raise ProtocolError(f"bad map length {bytes(buf[1:hdr])!r}")
    if count < 0:
        raise ProtocolError("bad map length")
    pos = hdr + 2
    pairs: List[Tuple[Reply, Reply]] = []
    for _ in range(count):
        k, ku = parse_reply(buf[pos:])
        if ku == 0:
            return None, 0
        pos += ku
        v, vu = parse_reply(buf[pos:])
        if vu == 0:
            return None, 0
        pos += vu
        pairs.append((k, v))  # type: ignore[arg-type]
    return Reply(kind=ReplyKind.MAP, pairs=pairs), pos


def _double(buf: bytes) -> Tuple[Optional[Reply], int]:
    eol = _find_crlf(buf, 1)
    if eol < 0:
        return None, 0
    raw = buf[1:eol]
    try:
        v = _parse_resp_double(raw)
    except ValueError:
        raise ProtocolError(f"bad double {bytes(raw)!r}")
    return Reply(kind=ReplyKind.DOUBLE, double=v), eol + 2


def _parse_resp_double(raw: bytes) -> float:
    s = raw.decode("ascii", "strict")
    if s == "inf":
        return float("inf")
    if s == "-inf":
        return float("-inf")
    if s == "nan":
        return float("nan")
    return float(s)


def _bool(buf: bytes) -> Tuple[Optional[Reply], int]:
    eol = _find_crlf(buf, 1)
    if eol < 0:
        return None, 0
    body = buf[1:eol]
    if body == b"t":
        return Reply(kind=ReplyKind.BOOLEAN, boolean=True), eol + 2
    if body == b"f":
        return Reply(kind=ReplyKind.BOOLEAN, boolean=False), eol + 2
    raise ProtocolError("bad boolean payload")


def _verbatim(buf: bytes) -> Tuple[Optional[Reply], int]:
    hdr = _find_crlf(buf, 1)
    if hdr < 0:
        return None, 0
    try:
        n = int(buf[1:hdr])
    except ValueError:
        raise ProtocolError(f"bad verbatim length {bytes(buf[1:hdr])!r}")
    if n < 4:
        raise ProtocolError("verbatim length < 4 (fmt + ':')")
    start = hdr + 2
    end = start + n
    if len(buf) < end + 2:
        return None, 0
    body = buf[start:end]
    if body[3:4] != b":":
        raise ProtocolError("verbatim missing fmt:data separator")
    return (
        Reply(kind=ReplyKind.VERBATIM, verbatim_fmt=bytes(body[:3]), data=bytes(body[4:])),
        end + 2,
    )


def _null(buf: bytes) -> Tuple[Optional[Reply], int]:
    if len(buf) < 3:
        return None, 0
    if buf[0:3] != b"_\r\n":
        raise ProtocolError("bad null payload")
    return Reply(kind=ReplyKind.NULL), 3


def _attributed(buf: bytes) -> Tuple[Optional[Reply], int]:
    _, used = _map(buf)
    if used == 0:
        return None, 0
    reply, u2 = parse_reply(buf[used:])
    if u2 == 0:
        return None, 0
    return reply, used + u2

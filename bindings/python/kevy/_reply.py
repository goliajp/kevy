"""The universal decoded RESP reply and a RESP2+RESP3 parser (contract §4.1).

``Reply`` covers every RESP2 and RESP3 shape. ``parse_reply`` decodes one
reply from a buffer starting at an offset and reports the new absolute
position (``== pos`` means "need more bytes"), so the same routine serves the
self-contained FFI path and the streaming TCP path. The parser walks the
buffer by index — it never slices a sub-buffer, so a deeply nested or
many-element reply costs O(N) not O(N²). It transparently skips attribute
frames and caps header-declared counts by the bytes remaining (fuzz-hardened,
mirroring the Rust parser).
"""

from __future__ import annotations

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


#: Shared empty aggregate children — scalar replies keep these instead of
#: allocating a fresh list per reply (only real aggregates build one).
_NO_ITEMS: Tuple["Reply", ...] = ()
_NO_PAIRS: Tuple[Tuple["Reply", "Reply"], ...] = ()


class Reply:
    """One decoded RESP value (contract §4.1).

    ``__slots__`` keeps each reply lean (no per-instance ``__dict__``);
    ``items``/``pairs`` default to a shared empty tuple, so a scalar reply
    allocates nothing beyond the object itself.
    """

    __slots__ = (
        "kind",
        "data",
        "integer",
        "double",
        "boolean",
        "verbatim_fmt",
        "items",
        "pairs",
    )

    def __init__(
        self,
        kind: ReplyKind,
        data: bytes = b"",
        integer: int = 0,
        double: float = 0.0,
        boolean: bool = False,
        verbatim_fmt: bytes = b"",
        items=_NO_ITEMS,
        pairs=_NO_PAIRS,
    ):
        self.kind = kind
        #: payload bytes for SIMPLE / ERROR / BULK / BIG_NUMBER / BLOB_ERROR /
        #: VERBATIM (data body).
        self.data = data
        self.integer = integer
        self.double = double
        self.boolean = boolean
        self.verbatim_fmt = verbatim_fmt  # 3-char format tag for VERBATIM
        #: elements for ARRAY / SET / PUSH.
        self.items = items
        #: (key, value) pairs for MAP.
        self.pairs = pairs

    def is_error(self) -> bool:
        return self.kind in (ReplyKind.ERROR, ReplyKind.BLOB_ERROR)

    def is_nil(self) -> bool:
        return self.kind in (ReplyKind.NIL, ReplyKind.NULL)

    def text(self) -> str:
        return self.data.decode("utf-8", "replace")

    def shape(self) -> str:
        return self.kind.value

    def __repr__(self) -> str:
        return f"Reply({self.kind.name})"


def decode_reply(buf: bytes) -> Reply:
    """Parse exactly one complete reply from a self-contained buffer (the
    FFI path). Raises ProtocolError on a truncated or malformed frame."""
    reply, pos = parse_reply(buf)
    if reply is None:
        raise ProtocolError("truncated RESP reply")
    return reply


# RESP tag bytes as ints, so the leaf dispatch compares ``buf[pos]`` (an int)
# without materialising a 1-byte slice.
_PLUS = ord("+")
_MINUS = ord("-")
_COLON = ord(":")
_DOLLAR = ord("$")
_STAR = ord("*")
_PERCENT = ord("%")
_TILDE = ord("~")
_COMMA = ord(",")
_HASH = ord("#")
_EQ = ord("=")
_LPAREN = ord("(")
_UNDER = ord("_")
_GT = ord(">")
_BANG = ord("!")
_PIPE = ord("|")


def _find_crlf(buf, start: int) -> int:
    return buf.find(b"\r\n", start)


def parse_reply(buf, pos: int = 0) -> Tuple[Optional[Reply], int]:
    """Decode one reply from ``buf`` starting at absolute offset ``pos``.

    Returns ``(reply, new_pos)`` where ``new_pos`` is the absolute offset just
    past the consumed frame. ``reply is None`` (with ``new_pos == pos``) means
    "need more bytes". Raises ProtocolError on a malformed frame. Speaks
    RESP2 + RESP3 and skips attribute frames. ``buf`` may be ``bytes`` or a
    ``bytearray`` — never sliced, only indexed."""
    if pos >= len(buf):
        return None, pos
    tag = buf[pos]
    if tag == _PLUS:
        return _line(buf, pos, ReplyKind.SIMPLE)
    if tag == _MINUS:
        return _line(buf, pos, ReplyKind.ERROR)
    if tag == _COLON:
        return _int(buf, pos)
    if tag == _DOLLAR:
        return _bulk(buf, pos, ReplyKind.BULK)
    if tag == _STAR:
        return _agg(buf, pos, ReplyKind.ARRAY)
    if tag == _PERCENT:
        return _map(buf, pos)
    if tag == _TILDE:
        return _agg(buf, pos, ReplyKind.SET)
    if tag == _COMMA:
        return _double(buf, pos)
    if tag == _HASH:
        return _bool(buf, pos)
    if tag == _EQ:
        return _verbatim(buf, pos)
    if tag == _LPAREN:
        return _line(buf, pos, ReplyKind.BIG_NUMBER)
    if tag == _UNDER:
        return _null(buf, pos)
    if tag == _GT:
        return _agg(buf, pos, ReplyKind.PUSH)
    if tag == _BANG:
        return _bulk(buf, pos, ReplyKind.BLOB_ERROR)
    if tag == _PIPE:
        return _attributed(buf, pos)
    raise ProtocolError(f"unknown RESP tag {bytes(buf[pos:pos + 1])!r}")


def _line(buf, pos: int, kind: ReplyKind) -> Tuple[Optional[Reply], int]:
    eol = _find_crlf(buf, pos + 1)
    if eol < 0:
        return None, pos
    return Reply(kind=kind, data=bytes(buf[pos + 1:eol])), eol + 2


def _int(buf, pos: int) -> Tuple[Optional[Reply], int]:
    eol = _find_crlf(buf, pos + 1)
    if eol < 0:
        return None, pos
    try:
        n = int(buf[pos + 1:eol])
    except ValueError:
        raise ProtocolError(f"bad integer reply {bytes(buf[pos + 1:eol])!r}")
    return Reply(kind=ReplyKind.INT, integer=n), eol + 2


def _bulk(buf, pos: int, kind: ReplyKind) -> Tuple[Optional[Reply], int]:
    hdr = _find_crlf(buf, pos + 1)
    if hdr < 0:
        return None, pos
    try:
        n = int(buf[pos + 1:hdr])
    except ValueError:
        raise ProtocolError(f"bad bulk length {bytes(buf[pos + 1:hdr])!r}")
    if n < 0:
        return Reply(kind=ReplyKind.NIL), hdr + 2
    start = hdr + 2
    end = start + n
    if len(buf) < end + 2:
        return None, pos
    return Reply(kind=kind, data=bytes(buf[start:end])), end + 2


def _agg(buf, pos: int, kind: ReplyKind) -> Tuple[Optional[Reply], int]:
    hdr = _find_crlf(buf, pos + 1)
    if hdr < 0:
        return None, pos
    try:
        count = int(buf[pos + 1:hdr])
    except ValueError:
        raise ProtocolError(f"bad aggregate length {bytes(buf[pos + 1:hdr])!r}")
    if count < 0:
        if kind == ReplyKind.PUSH:
            raise ProtocolError("push frame cannot be null")
        return Reply(kind=ReplyKind.NIL), hdr + 2
    cur = hdr + 2
    items: List[Reply] = []
    for _ in range(count):
        item, cur = parse_reply(buf, cur)
        if item is None:
            return None, pos
        items.append(item)
    return Reply(kind=kind, items=items), cur


def _map(buf, pos: int) -> Tuple[Optional[Reply], int]:
    hdr = _find_crlf(buf, pos + 1)
    if hdr < 0:
        return None, pos
    try:
        count = int(buf[pos + 1:hdr])
    except ValueError:
        raise ProtocolError(f"bad map length {bytes(buf[pos + 1:hdr])!r}")
    if count < 0:
        raise ProtocolError("bad map length")
    cur = hdr + 2
    pairs: List[Tuple[Reply, Reply]] = []
    for _ in range(count):
        k, cur = parse_reply(buf, cur)
        if k is None:
            return None, pos
        v, cur = parse_reply(buf, cur)
        if v is None:
            return None, pos
        pairs.append((k, v))
    return Reply(kind=ReplyKind.MAP, pairs=pairs), cur


def _double(buf, pos: int) -> Tuple[Optional[Reply], int]:
    eol = _find_crlf(buf, pos + 1)
    if eol < 0:
        return None, pos
    raw = buf[pos + 1:eol]
    try:
        v = _parse_resp_double(raw)
    except ValueError:
        raise ProtocolError(f"bad double {bytes(raw)!r}")
    return Reply(kind=ReplyKind.DOUBLE, double=v), eol + 2


def _parse_resp_double(raw) -> float:
    s = bytes(raw).decode("ascii", "strict")
    if s == "inf":
        return float("inf")
    if s == "-inf":
        return float("-inf")
    if s == "nan":
        return float("nan")
    return float(s)


def _bool(buf, pos: int) -> Tuple[Optional[Reply], int]:
    eol = _find_crlf(buf, pos + 1)
    if eol < 0:
        return None, pos
    body = buf[pos + 1:eol]
    if body == b"t":
        return Reply(kind=ReplyKind.BOOLEAN, boolean=True), eol + 2
    if body == b"f":
        return Reply(kind=ReplyKind.BOOLEAN, boolean=False), eol + 2
    raise ProtocolError("bad boolean payload")


def _verbatim(buf, pos: int) -> Tuple[Optional[Reply], int]:
    hdr = _find_crlf(buf, pos + 1)
    if hdr < 0:
        return None, pos
    try:
        n = int(buf[pos + 1:hdr])
    except ValueError:
        raise ProtocolError(f"bad verbatim length {bytes(buf[pos + 1:hdr])!r}")
    if n < 4:
        raise ProtocolError("verbatim length < 4 (fmt + ':')")
    start = hdr + 2
    end = start + n
    if len(buf) < end + 2:
        return None, pos
    if buf[start + 3] != _COLON:
        raise ProtocolError("verbatim missing fmt:data separator")
    return (
        Reply(
            kind=ReplyKind.VERBATIM,
            verbatim_fmt=bytes(buf[start:start + 3]),
            data=bytes(buf[start + 4:end]),
        ),
        end + 2,
    )


def _null(buf, pos: int) -> Tuple[Optional[Reply], int]:
    if len(buf) < pos + 3:
        return None, pos
    if buf[pos:pos + 3] != b"_\r\n":
        raise ProtocolError("bad null payload")
    return Reply(kind=ReplyKind.NULL), pos + 3


def _attributed(buf, pos: int) -> Tuple[Optional[Reply], int]:
    _, after = _map(buf, pos)
    if after == pos:
        return None, pos
    reply, end = parse_reply(buf, after)
    if reply is None:
        return None, pos
    return reply, end

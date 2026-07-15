"""Pure Reply → typed-value decoders shared by the blocking and asyncio
faces (contract §3). Each maps one decoded RESP reply onto the family's
return shape, raising the right ``KevyError`` on an error frame or a shape
mismatch — so both faces agree byte-for-byte on results and errors."""

from __future__ import annotations

from typing import List, Optional, Union

from ._errors import ProtocolError, error_from_reply_text
from ._reply import Reply, ReplyKind

Bytesish = Union[bytes, bytearray, memoryview, str]


def to_bytes(x: Bytesish) -> bytes:
    """Accept bytes or str, never assume UTF-8 for user data (§7): a str is
    UTF-8 encoded, bytes-like is passed through."""
    if isinstance(x, bytes):
        return x
    if isinstance(x, str):
        return x.encode("utf-8")
    if isinstance(x, (bytearray, memoryview)):
        return bytes(x)
    raise TypeError(f"expected bytes or str, got {type(x).__name__}")


def reply_error(r: Reply):
    """A Reply::Error → the right KevyError (StoreError when recognised,
    else ProtocolError with the wire text verbatim, §2.2)."""
    return error_from_reply_text(r.data)


def unexpected(r: Reply) -> ProtocolError:
    return ProtocolError(f"unexpected RESP reply variant: {r.shape()}")


def raise_if_error(r: Reply) -> None:
    if r.is_error():
        raise reply_error(r)


def dec_ok(r: Reply) -> None:
    if r.kind is ReplyKind.SIMPLE and r.data == b"OK":
        return None
    raise_if_error(r)
    raise unexpected(r)


def dec_pong(r: Reply) -> None:
    if r.kind is ReplyKind.SIMPLE and r.data == b"PONG":
        return None
    raise_if_error(r)
    raise unexpected(r)


def dec_int(r: Reply) -> int:
    if r.kind is ReplyKind.INT:
        return r.integer
    raise_if_error(r)
    raise unexpected(r)


def dec_count(r: Reply) -> int:
    n = dec_int(r)
    if n < 0:
        raise ProtocolError(f"expected non-negative count, got {n}")
    return n


def dec_bool(r: Reply) -> bool:
    if r.kind is ReplyKind.INT:
        return r.integer == 1
    if r.kind is ReplyKind.BOOLEAN:
        return r.boolean
    raise_if_error(r)
    raise unexpected(r)


def dec_opt_bulk(r: Reply) -> Optional[bytes]:
    if r.kind is ReplyKind.BULK:
        return r.data
    if r.is_nil():
        return None
    raise_if_error(r)
    raise unexpected(r)


def dec_str_simple(r: Reply) -> str:
    if r.kind is ReplyKind.SIMPLE:
        return r.text()
    raise_if_error(r)
    raise unexpected(r)


def _bulk_or_none(it: Reply) -> Optional[bytes]:
    if it.kind in (ReplyKind.BULK, ReplyKind.SIMPLE):
        return it.data
    if it.is_nil():
        return None
    raise unexpected(it)


def dec_bulks(r: Reply) -> List[Optional[bytes]]:
    """An ARRAY/SET reply → a list of bulk bytes (None for nil elements)."""
    if r.kind in (ReplyKind.ARRAY, ReplyKind.SET):
        return [_bulk_or_none(it) for it in r.items]
    raise_if_error(r)
    raise unexpected(r)


def dec_bulks_strict(r: Reply) -> List[bytes]:
    """dec_bulks with no nils expected (drops any nils, mirrors MGET-less
    array families that never emit nil)."""
    return [b for b in dec_bulks(r) if b is not None]


def dec_list_pop(r: Reply) -> List[bytes]:
    """LPOP/RPOP count form: ARRAY of bulks, a single BULK, or Nil → []."""
    if r.kind in (ReplyKind.ARRAY, ReplyKind.SET):
        return [b for b in dec_bulks(r)]  # type: ignore[misc]
    if r.kind is ReplyKind.BULK:
        return [r.data]
    if r.is_nil():
        return []
    raise_if_error(r)
    raise unexpected(r)


def dec_opt_float(r: Reply) -> Optional[float]:
    if r.kind is ReplyKind.BULK:
        return parse_float(r.data)
    if r.kind is ReplyKind.DOUBLE:
        return r.double
    if r.is_nil():
        return None
    raise_if_error(r)
    raise unexpected(r)


def dec_int_list(r: Reply) -> List[int]:
    if r.kind is ReplyKind.ARRAY:
        out = []
        for it in r.items:
            if it.kind is not ReplyKind.INT:
                raise unexpected(it)
            out.append(it.integer)
        return out
    raise_if_error(r)
    raise unexpected(r)


def score_of(r: Reply) -> float:
    if r.kind in (ReplyKind.BULK, ReplyKind.SIMPLE):
        return parse_float(r.data)
    if r.kind is ReplyKind.DOUBLE:
        return r.double
    raise unexpected(r)


def parse_float(b: bytes) -> float:
    s = b.decode("ascii", "replace")
    try:
        if s == "inf":
            return float("inf")
        if s == "-inf":
            return float("-inf")
        if s == "nan":
            return float("nan")
        return float(s)
    except ValueError:
        raise ProtocolError(f"bad float reply: {s}")

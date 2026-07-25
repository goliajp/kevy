"""Transactions: MULTI / EXEC / DISCARD (+ WATCH), contract §3.12.

Remote-only. A ``Transaction`` abandoned without exec/discard MUST send an
implicit DISCARD so the socket is not left in MULTI mode — Python has no
deterministic destructors, so this is a context manager (``with``) and a
best-effort ``__del__``; dropping a live transaction without close is a bug.
``TransactionReplies`` is a typed cursor over a successful EXEC (§4.10).
"""

from __future__ import annotations

from typing import List, Optional

from ._decode import Bytesish, to_bytes
from ._errors import InvalidInputError, ProtocolError, error_from_reply_text
from ._reply import Reply, ReplyKind


class Transaction:
    """One in-flight MULTI block over a remote connection."""

    def __init__(self, conn):
        self._conn = conn
        self._live = True

    def __enter__(self) -> "Transaction":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    def __del__(self):
        try:
            self.close()
        except Exception:
            pass

    # --- queueing -------------------------------------------------------

    def queue(self, *parts: Bytesish) -> "Transaction":
        """Queue one raw command (verb + args); expects +QUEUED."""
        if not parts:
            raise InvalidInputError("queue needs at least a verb")
        return self._queue([to_bytes(p) for p in parts])

    def set(self, key: Bytesish, value: Bytesish) -> "Transaction":
        return self._queue([b"SET", to_bytes(key), to_bytes(value)])

    def get(self, key: Bytesish) -> "Transaction":
        return self._queue([b"GET", to_bytes(key)])

    def delete(self, *keys: Bytesish) -> "Transaction":
        if not keys:
            raise InvalidInputError("delete needs at least one key")
        return self._queue([b"DEL", *[to_bytes(k) for k in keys]])

    def exists(self, *keys: Bytesish) -> "Transaction":
        if not keys:
            raise InvalidInputError("exists needs at least one key")
        return self._queue([b"EXISTS", *[to_bytes(k) for k in keys]])

    def incr(self, key: Bytesish) -> "Transaction":
        return self._queue([b"INCR", to_bytes(key)])

    def incr_by(self, key: Bytesish, delta: int) -> "Transaction":
        return self._queue([b"INCRBY", to_bytes(key), str(int(delta)).encode()])

    def mget(self, *keys: Bytesish) -> "Transaction":
        if not keys:
            raise InvalidInputError("mget needs at least one key")
        return self._queue([b"MGET", *[to_bytes(k) for k in keys]])

    def mset(self, *pairs: Bytesish) -> "Transaction":
        pb = [to_bytes(p) for p in pairs]
        if not pb or len(pb) % 2 != 0:
            raise InvalidInputError("mset needs a non-empty even key/value list")
        return self._queue([b"MSET", *pb])

    def _queue(self, argv: List[bytes]) -> "Transaction":
        r = self._conn.request(argv)
        if r.kind is ReplyKind.SIMPLE and r.data == b"QUEUED":
            return self
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"expected +QUEUED, got {r.shape()}")

    # --- execution ------------------------------------------------------

    def _finish(self) -> Reply:
        self._live = False
        return self._conn.request([b"EXEC"])

    def exec(self) -> List[Reply]:
        """EXEC → per-command replies. A WATCH abort (Nil) collapses to []
        (legacy); prefer exec_watched for new code."""
        r = self._finish()
        if r.kind is ReplyKind.ARRAY:
            return r.items
        if r.is_nil():
            return []
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"EXEC: unexpected {r.shape()}")

    def exec_watched(self) -> Optional[List[Reply]]:
        """None on WATCH abort; [] on empty success."""
        r = self._finish()
        if r.kind is ReplyKind.ARRAY:
            return r.items
        if r.is_nil():
            return None
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"EXEC: unexpected {r.shape()}")

    def exec_typed(self) -> "TransactionReplies":
        """A typed reply cursor; a WATCH abort is a ProtocolError."""
        r = self._finish()
        if r.kind is ReplyKind.ARRAY:
            return TransactionReplies(r.items)
        if r.is_nil():
            raise ProtocolError("transaction aborted by WATCH")
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"EXEC: unexpected {r.shape()}")

    def exec_watched_typed(self) -> Optional["TransactionReplies"]:
        r = self._finish()
        if r.kind is ReplyKind.ARRAY:
            return TransactionReplies(r.items)
        if r.is_nil():
            return None
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"EXEC: unexpected {r.shape()}")

    def discard(self) -> None:
        """Abandon the queued commands."""
        if not self._live:
            return
        self._live = False
        r = self._conn.request([b"DISCARD"])
        if r.is_error():
            raise error_from_reply_text(r.data)

    def close(self) -> None:
        """Send an implicit DISCARD if still live, so the socket is not left
        in MULTI mode. Idempotent."""
        if self._live:
            self._live = False
            try:
                self._conn.request([b"DISCARD"])
            except Exception:
                pass


class TransactionReplies:
    """A typed cursor over a successful EXEC's per-command replies (§4.10).
    Each ``next_*`` consumes one reply; a variant mismatch is a
    ProtocolError and the cursor still advances so ``expect_empty`` stays
    meaningful."""

    def __init__(self, items: List[Reply]):
        self._items = items
        self._pos = 0

    @property
    def remaining(self) -> int:
        return len(self._items) - self._pos

    def expect_empty(self) -> None:
        left = self.remaining
        if left != 0:
            raise ProtocolError(f"transaction reply cursor has {left} un-consumed replies")

    def raw(self) -> Reply:
        if self._pos >= len(self._items):
            raise ProtocolError("transaction reply cursor exhausted")
        v = self._items[self._pos]
        self._pos += 1
        return v

    def next_ok(self) -> None:
        v = self.raw()
        if v.kind is ReplyKind.SIMPLE and v.data == b"OK":
            return
        raise _mismatch("Simple(OK)", v)

    def next_ok_or_nil(self) -> bool:
        v = self.raw()
        if v.kind is ReplyKind.SIMPLE and v.data == b"OK":
            return True
        if v.is_nil():
            return False
        raise _mismatch("Simple(OK) or Nil", v)

    def next_int(self) -> int:
        v = self.raw()
        if v.kind is ReplyKind.INT:
            return v.integer
        raise _mismatch("Int", v)

    def next_bulk(self) -> Optional[bytes]:
        v = self.raw()
        if v.kind is ReplyKind.BULK:
            return v.data
        if v.is_nil():
            return None
        raise _mismatch("Bulk or Nil", v)

    def next_array_of_bulks(self) -> List[Optional[bytes]]:
        v = self.raw()
        if v.kind is ReplyKind.ARRAY:
            out: List[Optional[bytes]] = []
            for it in v.items:
                if it.kind is ReplyKind.BULK:
                    out.append(it.data)
                elif it.is_nil():
                    out.append(None)
                else:
                    raise _mismatch("Array element Bulk/Nil", it)
            return out
        if v.is_nil():
            return []
        raise _mismatch("Array", v)

    def next_simple(self) -> bytes:
        v = self.raw()
        if v.kind is ReplyKind.SIMPLE:
            return v.data
        raise _mismatch("Simple", v)


def _mismatch(want: str, got: Reply) -> ProtocolError:
    return ProtocolError(f"transaction reply mismatch: expected {want}, got {got.shape()}")

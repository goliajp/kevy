"""Asyncio transaction handle (contract §3.12, async face).

The coroutine twin of :class:`kevy._transaction.Transaction`, reusing the
pure :class:`kevy._transaction.TransactionReplies` cursor. Use ``async with``
(or ``await tx.close()``) so an abandoned transaction sends its implicit
DISCARD — Python has no deterministic destructors.
"""

from __future__ import annotations

from typing import List, Optional

from ._decode import Bytesish, to_bytes
from ._errors import InvalidInputError, ProtocolError, error_from_reply_text
from ._reply import Reply, ReplyKind
from ._transaction import TransactionReplies


class AsyncTransaction:
    def __init__(self, conn):
        self._conn = conn
        self._live = True

    async def __aenter__(self) -> "AsyncTransaction":
        return self

    async def __aexit__(self, *exc) -> None:
        await self.close()

    async def queue(self, *parts: Bytesish) -> "AsyncTransaction":
        if not parts:
            raise InvalidInputError("queue needs at least a verb")
        return await self._queue([to_bytes(p) for p in parts])

    async def set(self, key: Bytesish, value: Bytesish) -> "AsyncTransaction":
        return await self._queue([b"SET", to_bytes(key), to_bytes(value)])

    async def get(self, key: Bytesish) -> "AsyncTransaction":
        return await self._queue([b"GET", to_bytes(key)])

    async def delete(self, *keys: Bytesish) -> "AsyncTransaction":
        if not keys:
            raise InvalidInputError("delete needs at least one key")
        return await self._queue([b"DEL", *[to_bytes(k) for k in keys]])

    async def exists(self, *keys: Bytesish) -> "AsyncTransaction":
        if not keys:
            raise InvalidInputError("exists needs at least one key")
        return await self._queue([b"EXISTS", *[to_bytes(k) for k in keys]])

    async def incr(self, key: Bytesish) -> "AsyncTransaction":
        return await self._queue([b"INCR", to_bytes(key)])

    async def incr_by(self, key: Bytesish, delta: int) -> "AsyncTransaction":
        return await self._queue([b"INCRBY", to_bytes(key), str(int(delta)).encode()])

    async def mget(self, *keys: Bytesish) -> "AsyncTransaction":
        if not keys:
            raise InvalidInputError("mget needs at least one key")
        return await self._queue([b"MGET", *[to_bytes(k) for k in keys]])

    async def mset(self, *pairs: Bytesish) -> "AsyncTransaction":
        pb = [to_bytes(p) for p in pairs]
        if not pb or len(pb) % 2 != 0:
            raise InvalidInputError("mset needs a non-empty even key/value list")
        return await self._queue([b"MSET", *pb])

    async def _queue(self, argv: List[bytes]) -> "AsyncTransaction":
        r = await self._conn.request(argv)
        if r.kind is ReplyKind.SIMPLE and r.data == b"QUEUED":
            return self
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"expected +QUEUED, got {r.shape()}")

    async def _finish(self) -> Reply:
        self._live = False
        return await self._conn.request([b"EXEC"])

    async def exec(self) -> List[Reply]:
        r = await self._finish()
        if r.kind is ReplyKind.ARRAY:
            return r.items
        if r.is_nil():
            return []
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"EXEC: unexpected {r.shape()}")

    async def exec_watched(self) -> Optional[List[Reply]]:
        r = await self._finish()
        if r.kind is ReplyKind.ARRAY:
            return r.items
        if r.is_nil():
            return None
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"EXEC: unexpected {r.shape()}")

    async def exec_typed(self) -> TransactionReplies:
        r = await self._finish()
        if r.kind is ReplyKind.ARRAY:
            return TransactionReplies(r.items)
        if r.is_nil():
            raise ProtocolError("transaction aborted by WATCH")
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"EXEC: unexpected {r.shape()}")

    async def exec_watched_typed(self) -> Optional[TransactionReplies]:
        r = await self._finish()
        if r.kind is ReplyKind.ARRAY:
            return TransactionReplies(r.items)
        if r.is_nil():
            return None
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"EXEC: unexpected {r.shape()}")

    async def discard(self) -> None:
        if not self._live:
            return
        self._live = False
        r = await self._conn.request([b"DISCARD"])
        if r.is_error():
            raise error_from_reply_text(r.data)

    async def close(self) -> None:
        if self._live:
            self._live = False
            try:
                await self._conn.request([b"DISCARD"])
            except Exception:
                pass

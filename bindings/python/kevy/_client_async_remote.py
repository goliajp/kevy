"""Async remote-facing and backend-aware families (contract §3.8, §3.10,
§3.12–§3.14): the coroutine mirror of :class:`kevy._client_remote.RemoteMixin`.
"""

from __future__ import annotations

import asyncio
from typing import List, Optional, Sequence, Tuple

from ._decode import Bytesish, score_of, to_bytes
from ._enums import IdxType
from ._errors import (
    InvalidInputError,
    ProtocolError,
    UnsupportedError,
    error_from_reply_text,
)
from ._client_remote import (
    _block_argv,
    _check_block,
    _embedded_feed_guard,
    _expect_count,
    _expect_ok,
)
from ._ops import seconds_of
from ._parse import (
    knn_blob,
    parse_feed_batch,
    parse_idx_list,
    parse_idx_page,
    parse_ranked,
)
from ._pipeline import PipelineBuf
from ._reply import Reply, ReplyKind
from ._transaction_async import AsyncTransaction
from ._types import FeedBatch, IdxInfo, IdxPage, Ranked


class AsyncRemoteMixin:
    _remote: object
    _emb: object

    def _require_remote(self, feature: str):
        if getattr(self, "_remote", None) is not None:
            return self._remote
        raise UnsupportedError(
            f"{feature} is remote-only; on the embedded backend use the raw cmd() path"
        )

    async def _exec(self, argv: List[bytes]) -> Reply:  # provided by AsyncClient
        raise NotImplementedError

    # --- IDX.* (§3.8) ---------------------------------------------------

    async def idx_create_range(self, name, prefix, field, ty: IdxType) -> None:
        await self.idx_create_raw(
            to_bytes(name), b"ON", b"PREFIX", to_bytes(prefix),
            b"FIELD", to_bytes(field), b"TYPE", ty.tag(), b"KIND", b"range",
        )

    async def idx_create_raw(self, *args: Bytesish) -> None:
        self._require_remote("IDX.CREATE")
        _expect_ok(await self._exec([b"IDX.CREATE", *[to_bytes(a) for a in args]]))

    async def idx_drop(self, name: Bytesish) -> bool:
        self._require_remote("IDX.DROP")
        r = await self._exec([b"IDX.DROP", to_bytes(name)])
        if r.kind is ReplyKind.INT:
            return r.integer == 1
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"IDX.DROP: unexpected {r.shape()}")

    async def idx_list(self) -> List[IdxInfo]:
        self._require_remote("IDX.LIST")
        return parse_idx_list(await self._exec([b"IDX.LIST"]))

    async def idx_query_range(self, name, min_, max_, limit: int, cursor: Optional[bytes] = None) -> IdxPage:
        args = [b"RANGE", to_bytes(min_), to_bytes(max_), b"LIMIT", str(int(limit)).encode()]
        if cursor is not None:
            args += [b"CURSOR", to_bytes(cursor)]
        return parse_idx_page(await self._idx_query(name, args))

    async def idx_query_eq(self, name, value, limit: int) -> IdxPage:
        args = [b"EQ", to_bytes(value), b"LIMIT", str(int(limit)).encode()]
        return parse_idx_page(await self._idx_query(name, args))

    async def idx_query_match(self, name, text, limit: int) -> List[Ranked]:
        args = [b"MATCH", to_bytes(text), b"LIMIT", str(int(limit)).encode()]
        return parse_ranked(await self._idx_query(name, args))

    async def idx_query_knn(self, name, vector: Sequence[float], k: int) -> List[Ranked]:
        args = [b"KNN", knn_blob(list(vector)), b"LIMIT", str(int(k)).encode()]
        return parse_ranked(await self._idx_query(name, args))

    async def idx_query_raw(self, *args: Bytesish) -> Reply:
        self._require_remote("IDX.QUERY")
        r = await self._exec([b"IDX.QUERY", *[to_bytes(a) for a in args]])
        if r.is_error():
            raise error_from_reply_text(r.data)
        return r

    async def _idx_query(self, name, args: List[bytes]) -> Reply:
        return await self.idx_query_raw(to_bytes(name), *args)

    # --- FEED.* (§3.10) -------------------------------------------------

    async def feed_shards(self) -> int:
        if getattr(self, "_emb", None) is not None:
            return 1
        return _expect_count(await self._exec([b"FEED.SHARDS"]))

    async def feed_tail(self, shard: int) -> Tuple[int, int]:
        if getattr(self, "_emb", None) is not None:
            _embedded_feed_guard(shard)
        r = await self._exec([b"FEED.TAIL", str(int(shard)).encode()])
        if r.kind is ReplyKind.ARRAY and len(r.items) == 2 and all(
            it.kind is ReplyKind.INT for it in r.items
        ):
            return r.items[0].integer, r.items[1].integer
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"FEED.TAIL: unexpected {r.shape()}")

    async def feed_read(self, shard: int, generation: int, offset: int,
                        count: Optional[int] = None, prefixes: Sequence[Bytesish] = ()) -> FeedBatch:
        if getattr(self, "_emb", None) is not None:
            _embedded_feed_guard(shard)
        argv = [b"FEED.READ", str(int(shard)).encode(), str(int(generation)).encode(), str(int(offset)).encode()]
        if count is not None and count > 0:
            argv += [b"COUNT", str(int(count)).encode()]
        for p in prefixes:
            argv += [b"PREFIX", to_bytes(p)]
        return parse_feed_batch(await self._exec(argv))

    # --- blocking pops (§3.14) -----------------------------------------

    async def blpop(self, keys: Sequence[Bytesish], timeout=None) -> Optional[Tuple[bytes, bytes]]:
        kb = _check_block(keys, timeout)
        if getattr(self, "_emb", None) is not None:
            return await self._emb_block_kv(b"LPOP", kb, timeout)
        return await self._pop_kv(b"BLPOP", kb, timeout)

    async def brpop(self, keys: Sequence[Bytesish], timeout=None) -> Optional[Tuple[bytes, bytes]]:
        kb = _check_block(keys, timeout)
        if getattr(self, "_emb", None) is not None:
            return await self._emb_block_kv(b"RPOP", kb, timeout)
        return await self._pop_kv(b"BRPOP", kb, timeout)

    async def bzpopmin(self, keys: Sequence[Bytesish], timeout=None) -> Optional[Tuple[bytes, bytes, float]]:
        kb = _check_block(keys, timeout)
        if getattr(self, "_emb", None) is not None:
            return await self._emb_block_z(kb, timeout)
        r = await self._exec(_block_argv(b"BZPOPMIN", kb, timeout))
        if r.kind is ReplyKind.ARRAY and len(r.items) == 3:
            k, m, s = r.items
            if k.kind is not ReplyKind.BULK or m.kind is not ReplyKind.BULK:
                raise ProtocolError("BZPOPMIN: bad hit shape")
            return (k.data, m.data, score_of(s))
        if r.is_nil():
            return None
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"BZPOPMIN: unexpected {r.shape()}")

    async def _pop_kv(self, verb: bytes, keys: List[bytes], timeout):
        r = await self._exec(_block_argv(verb, keys, timeout))
        if r.kind is ReplyKind.ARRAY and len(r.items) == 2:
            k, v = r.items
            if k.kind is not ReplyKind.BULK or v.kind is not ReplyKind.BULK:
                raise ProtocolError(f"{verb!r}: bad hit shape")
            return (k.data, v.data)
        if r.is_nil():
            return None
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"{verb!r}: unexpected {r.shape()}")

    async def _emb_block_kv(self, verb: bytes, keys: List[bytes], timeout):
        loop = asyncio.get_event_loop()
        deadline = None if timeout is None else loop.time() + seconds_of(timeout)
        while True:
            for k in keys:
                r = await self._exec([verb, k, b"1"])
                if r.kind is ReplyKind.ARRAY and r.items and r.items[0].kind is ReplyKind.BULK:
                    return (k, r.items[0].data)
                if r.kind is ReplyKind.BULK:
                    return (k, r.data)
            if deadline is not None and loop.time() >= deadline:
                return None
            await asyncio.sleep(0.005)

    async def _emb_block_z(self, keys: List[bytes], timeout):
        loop = asyncio.get_event_loop()
        deadline = None if timeout is None else loop.time() + seconds_of(timeout)
        while True:
            for k in keys:
                r = await self._exec([b"ZPOPMIN", k, b"1"])
                if r.kind is ReplyKind.ARRAY and len(r.items) >= 2 and r.items[0].kind is ReplyKind.BULK:
                    return (k, r.items[0].data, score_of(r.items[1]))
            if deadline is not None and loop.time() >= deadline:
                return None
            await asyncio.sleep(0.005)

    # --- transactions & pipeline (§3.12–§3.13) -------------------------

    async def watch(self, *keys: Bytesish) -> None:
        if not keys:
            raise InvalidInputError("watch needs at least one key")
        conn = self._require_remote("WATCH")
        _expect_ok(await conn.request([b"WATCH", *[to_bytes(k) for k in keys]]))

    async def unwatch(self) -> None:
        conn = self._require_remote("UNWATCH")
        _expect_ok(await conn.request([b"UNWATCH"]))

    async def multi(self) -> AsyncTransaction:
        conn = self._require_remote("MULTI/EXEC")
        _expect_ok(await conn.request([b"MULTI"]))
        return AsyncTransaction(conn)

    async def pipeline(self, build) -> List[Reply]:
        conn = self._require_remote("pipeline")
        buf = PipelineBuf()
        build(buf)
        if buf.poisoned:
            raise InvalidInputError("pipeline: a queued command had an empty argv")
        if buf.length == 0:
            return []
        return await conn.pipeline_raw(buf.wire, buf.length)

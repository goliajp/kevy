"""The unified asyncio client (contract §1.4).

``AsyncClient`` is the coroutine twin of :class:`kevy.Client`: the SAME
package ships both faces (redis-py ships ``redis`` + ``redis.asyncio`` the
same way). Every command is an ``async def``; both faces build identical
argv and share the pure decoders, so they agree on results. Remote uses
asyncio streams; embedded runs the in-process op in a worker thread so it
never blocks the event loop (§1.4 permits the async face to run the blocking
embedded op).
"""

from __future__ import annotations

import asyncio
from typing import List, Optional, Sequence

from . import _ops as ops
from ._client import ZArg, _norm_zmembers
from ._client_async_remote import AsyncRemoteMixin
from ._decode import Bytesish, to_bytes
from ._embedded import DB
from ._enums import HExpireCond, ZAggregate
from ._errors import ClosedError, InvalidInputError
from ._reply import Reply
from ._resp_conn_async import AsyncRespConn
from ._url import TargetKind, parse_connect_url, release_store, resolve_store


class AsyncClient(AsyncRemoteMixin):
    """An open asyncio connection to a kevy backend (§1.4)."""

    def __init__(self, remote: Optional[AsyncRespConn], emb: Optional[DB], emb_key: str, url: str):
        self._remote = remote
        self._emb = emb
        self._emb_key = emb_key
        self.url = url

    @classmethod
    async def connect(cls, url: str) -> "AsyncClient":
        target = parse_connect_url(url)
        if target.kind is TargetKind.REMOTE:
            conn = await AsyncRespConn.dial(target.host, target.port)
            if target.db is not None:
                try:
                    await conn.select_db(target.db)
                except Exception:
                    conn.close()
                    raise
            return cls(conn, None, "", url)
        db, key = await asyncio.to_thread(resolve_store, target)
        return cls(None, db, key, url)

    async def close(self) -> None:
        if self._remote is not None:
            self._remote.close()
            self._remote = None
        if self._emb is not None:
            db, key = self._emb, self._emb_key
            self._emb = None
            await asyncio.to_thread(release_store, key, db)

    async def __aenter__(self) -> "AsyncClient":
        return self

    async def __aexit__(self, *exc) -> None:
        await self.close()

    @property
    def is_embedded(self) -> bool:
        return self._emb is not None

    # --- transport ------------------------------------------------------

    async def _exec(self, argv: List[bytes]) -> Reply:
        if self._emb is not None:
            return await asyncio.to_thread(self._emb.cmd, *argv)
        if self._remote is not None:
            return await self._remote.request(argv)
        raise ClosedError()

    async def _run(self, built):
        argv, dec = built
        return dec(await self._exec(argv))

    # --- raw escape hatch (§7) -----------------------------------------

    async def do(self, *argv: Bytesish) -> Reply:
        if not argv:
            raise InvalidInputError("do needs at least a verb")
        return await self._exec([to_bytes(a) for a in argv])

    cmd = do

    # --- core string / generic (§3.1) ----------------------------------

    async def ping(self) -> None:
        if self._emb is not None:
            return None
        await self._run(ops.ping())

    async def set(self, key: Bytesish, value: Bytesish) -> None:
        await self._run(ops.set_(key, value))

    async def get(self, key: Bytesish) -> Optional[bytes]:
        return await self._run(ops.get(key))

    async def delete(self, *keys: Bytesish) -> int:
        return await self._run(ops.delete(keys))

    async def exists(self, *keys: Bytesish) -> int:
        return await self._run(ops.exists(keys))

    async def incr(self, key: Bytesish) -> int:
        return await self._run(ops.incr(key))

    async def incr_by(self, key: Bytesish, delta: int) -> int:
        return await self._run(ops.incr_by(key, delta))

    async def expire(self, key: Bytesish, ttl) -> bool:
        return await self._run(ops.expire(key, ttl))

    async def persist(self, key: Bytesish) -> bool:
        return await self._run(ops.persist(key))

    async def ttl_ms(self, key: Bytesish) -> int:
        return await self._run(ops.ttl_ms(key))

    async def type_of(self, key: Bytesish) -> str:
        return await self._run(ops.type_of(key))

    async def dbsize(self) -> int:
        return await self._run(ops.dbsize())

    async def flushall(self) -> None:
        await self._run(ops.flushall())

    async def set_with_ttl(self, key: Bytesish, value: Bytesish, ttl) -> None:
        await self._run(ops.set_with_ttl(key, value, ttl))

    async def mget(self, *keys: Bytesish) -> List[Optional[bytes]]:
        return await self._run(ops.mget(keys))

    async def mset(self, *pairs: Bytesish) -> None:
        await self._run(ops.mset(pairs))

    async def publish(self, channel: Bytesish, message: Bytesish) -> int:
        return await self._run(ops.publish(channel, message))

    # --- hash (§3.2) ----------------------------------------------------

    async def hset(self, key: Bytesish, *pairs: Bytesish) -> int:
        return await self._run(ops.hset(key, pairs))

    async def hget(self, key: Bytesish, field: Bytesish) -> Optional[bytes]:
        return await self._run(ops.hget(key, field))

    async def hdel(self, key: Bytesish, *fields: Bytesish) -> int:
        return await self._run(ops.hdel(key, fields))

    async def hlen(self, key: Bytesish) -> int:
        return await self._run(ops.hlen(key))

    async def hgetall(self, key: Bytesish) -> List[Optional[bytes]]:
        return await self._run(ops.hgetall(key))

    async def hkeys(self, key: Bytesish) -> List[Optional[bytes]]:
        return await self._run(ops.hkeys(key))

    async def hvals(self, key: Bytesish) -> List[Optional[bytes]]:
        return await self._run(ops.hvals(key))

    # --- list (§3.3) ----------------------------------------------------

    async def lpush(self, key: Bytesish, *values: Bytesish) -> int:
        return await self._run(ops.lpush(key, values))

    async def rpush(self, key: Bytesish, *values: Bytesish) -> int:
        return await self._run(ops.rpush(key, values))

    async def lpop(self, key: Bytesish, count: int = 1) -> List[bytes]:
        return await self._run(ops.lpop(key, count))

    async def rpop(self, key: Bytesish, count: int = 1) -> List[bytes]:
        return await self._run(ops.rpop(key, count))

    async def llen(self, key: Bytesish) -> int:
        return await self._run(ops.llen(key))

    async def lrange(self, key: Bytesish, start: int, stop: int) -> List[bytes]:
        return await self._run(ops.lrange(key, start, stop))

    # --- set (§3.4) -----------------------------------------------------

    async def sadd(self, key: Bytesish, *members: Bytesish) -> int:
        return await self._run(ops.sadd(key, members))

    async def srem(self, key: Bytesish, *members: Bytesish) -> int:
        return await self._run(ops.srem(key, members))

    async def smembers(self, key: Bytesish) -> List[bytes]:
        return await self._run(ops.smembers(key))

    async def scard(self, key: Bytesish) -> int:
        return await self._run(ops.scard(key))

    async def sismember(self, key: Bytesish, member: Bytesish) -> bool:
        return await self._run(ops.sismember(key, member))

    async def sinter(self, *keys: Bytesish) -> List[bytes]:
        return await self._run(ops.sinter(keys))

    async def sunion(self, *keys: Bytesish) -> List[bytes]:
        return await self._run(ops.sunion(keys))

    async def sdiff(self, *keys: Bytesish) -> List[bytes]:
        return await self._run(ops.sdiff(keys))

    # --- sorted set (§3.5) ---------------------------------------------

    async def zadd(self, key: Bytesish, *members: ZArg) -> int:
        return await self._run(ops.zadd(key, _norm_zmembers(members)))

    async def zrem(self, key: Bytesish, *members: Bytesish) -> int:
        return await self._run(ops.zrem(key, members))

    async def zscore(self, key: Bytesish, member: Bytesish) -> Optional[float]:
        return await self._run(ops.zscore(key, member))

    async def zcard(self, key: Bytesish) -> int:
        return await self._run(ops.zcard(key))

    async def zrange(self, key: Bytesish, start: int, stop: int) -> List[bytes]:
        return await self._run(ops.zrange(key, start, stop))

    # --- sorted-set algebra (§3.6) -------------------------------------

    async def zinterstore(self, dest: Bytesish, *keys: Bytesish) -> int:
        return await self._run(ops.zinterstore(dest, keys))

    async def zinterstore_with(self, dest, keys, weights=None, aggregate=ZAggregate.SUM) -> int:
        return await self._run(ops.zinterstore(dest, keys, weights, aggregate))

    async def zunionstore(self, dest: Bytesish, *keys: Bytesish) -> int:
        return await self._run(ops.zunionstore(dest, keys))

    async def zunionstore_with(self, dest, keys, weights=None, aggregate=ZAggregate.SUM) -> int:
        return await self._run(ops.zunionstore(dest, keys, weights, aggregate))

    async def zintercard(self, keys, limit: Optional[int] = None) -> int:
        return await self._run(ops.zintercard(keys, limit))

    # --- hash-field TTL (§3.7) -----------------------------------------

    async def hexpire(self, key, fields, ttl, cond=HExpireCond.ALWAYS) -> List[int]:
        return await self._run(ops.hexpire(key, fields, ttl, cond))

    async def hpexpire(self, key, fields, ttl, cond=HExpireCond.ALWAYS) -> List[int]:
        return await self._run(ops.hpexpire(key, fields, ttl, cond))

    async def hpersist(self, key, *fields: Bytesish) -> List[int]:
        return await self._run(ops.hpersist(key, fields))

    async def httl(self, key, *fields: Bytesish) -> List[int]:
        return await self._run(ops.httl(key, fields))

    async def hpttl(self, key, *fields: Bytesish) -> List[int]:
        return await self._run(ops.hpttl(key, fields))

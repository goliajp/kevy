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
from ._client import Duration, ZArg, _norm_zmembers
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
        """Round-trip liveness check; raises on a non-PONG (remote only)."""
        if self._emb is not None:
            return None
        await self._run(ops.ping())

    async def set(self, key: Bytesish, value: Bytesish) -> None:
        """Store ``value`` at ``key`` (no TTL); raises on a non-OK reply."""
        await self._run(ops.set_(key, value))

    async def get(self, key: Bytesish) -> Optional[bytes]:
        """Value at ``key``, or None on a miss."""
        return await self._run(ops.get(key))

    async def delete(self, *keys: Bytesish) -> int:
        """Delete the given keys; returns how many existed."""
        return await self._run(ops.delete(keys))

    async def exists(self, *keys: Bytesish) -> int:
        """Count how many of the given keys exist (counts duplicates)."""
        return await self._run(ops.exists(keys))

    async def incr(self, key: Bytesish) -> int:
        """Increment by one; returns the new value. NotIntegerError if unparsable."""
        return await self._run(ops.incr(key))

    async def incr_by(self, key: Bytesish, delta: int) -> int:
        """Increment by ``delta``; returns the new value. NotIntegerError if unparsable."""
        return await self._run(ops.incr_by(key, delta))

    async def expire(self, key: Bytesish, ttl: Duration) -> bool:
        """Set a TTL on ``key``; returns True if the key existed."""
        return await self._run(ops.expire(key, ttl))

    async def persist(self, key: Bytesish) -> bool:
        """Clear any TTL; returns True if a TTL was removed."""
        return await self._run(ops.persist(key))

    async def ttl_ms(self, key: Bytesish) -> int:
        """Remaining TTL in ms; -1 if no TTL, -2 if the key is absent."""
        return await self._run(ops.ttl_ms(key))

    async def type_of(self, key: Bytesish) -> str:
        """Type name of ``key`` ("string"/"list"/…), or "none" if absent."""
        return await self._run(ops.type_of(key))

    async def dbsize(self) -> int:
        """Number of keys in the current database."""
        return await self._run(ops.dbsize())

    async def flushall(self) -> None:
        """Delete every key in every database; raises on a non-OK reply."""
        await self._run(ops.flushall())

    async def set_with_ttl(self, key: Bytesish, value: Bytesish, ttl: Duration) -> None:
        """Store ``value`` at ``key`` with a TTL; raises on a non-OK reply."""
        await self._run(ops.set_with_ttl(key, value, ttl))

    async def mget(self, *keys: Bytesish) -> List[Optional[bytes]]:
        """Values for the given keys, in order; None for each miss."""
        return await self._run(ops.mget(keys))

    async def mset(self, *pairs: Bytesish) -> None:
        """Set key/value pairs (flat, even arity); InvalidInputError if odd."""
        await self._run(ops.mset(pairs))

    async def publish(self, channel: Bytesish, message: Bytesish) -> int:
        """Publish to ``channel``; returns the number of receivers."""
        return await self._run(ops.publish(channel, message))

    # --- hash (§3.2) ----------------------------------------------------

    async def hset(self, key: Bytesish, *pairs: Bytesish) -> int:
        """Set field/value pairs (flat, even arity); returns fields added."""
        return await self._run(ops.hset(key, pairs))

    async def hget(self, key: Bytesish, field: Bytesish) -> Optional[bytes]:
        """Value of ``field`` in the hash, or None on a miss."""
        return await self._run(ops.hget(key, field))

    async def hdel(self, key: Bytesish, *fields: Bytesish) -> int:
        """Delete the given fields; returns how many existed."""
        return await self._run(ops.hdel(key, fields))

    async def hlen(self, key: Bytesish) -> int:
        """Number of fields in the hash."""
        return await self._run(ops.hlen(key))

    async def hgetall(self, key: Bytesish) -> List[Optional[bytes]]:
        """Flat [field, value, …] list for the hash; empty if absent."""
        return await self._run(ops.hgetall(key))

    async def hkeys(self, key: Bytesish) -> List[Optional[bytes]]:
        """All field names in the hash; empty if absent."""
        return await self._run(ops.hkeys(key))

    async def hvals(self, key: Bytesish) -> List[Optional[bytes]]:
        """All field values in the hash; empty if absent."""
        return await self._run(ops.hvals(key))

    # --- list (§3.3) ----------------------------------------------------

    async def lpush(self, key: Bytesish, *values: Bytesish) -> int:
        """Prepend values (last arg ends up at the head); returns new length."""
        return await self._run(ops.lpush(key, values))

    async def rpush(self, key: Bytesish, *values: Bytesish) -> int:
        """Append values to the tail; returns the new length."""
        return await self._run(ops.rpush(key, values))

    async def lpop(self, key: Bytesish, count: int = 1) -> List[bytes]:
        """Pop up to ``count`` elements from the head; [] if empty."""
        return await self._run(ops.lpop(key, count))

    async def rpop(self, key: Bytesish, count: int = 1) -> List[bytes]:
        """Pop up to ``count`` elements from the tail; [] if empty."""
        return await self._run(ops.rpop(key, count))

    async def llen(self, key: Bytesish) -> int:
        """Length of the list; 0 if absent."""
        return await self._run(ops.llen(key))

    async def lrange(self, key: Bytesish, start: int, stop: int) -> List[bytes]:
        """Elements in the inclusive [start, stop] range (negatives count from the tail)."""
        return await self._run(ops.lrange(key, start, stop))

    # --- set (§3.4) -----------------------------------------------------

    async def sadd(self, key: Bytesish, *members: Bytesish) -> int:
        """Add members to the set; returns how many were new."""
        return await self._run(ops.sadd(key, members))

    async def srem(self, key: Bytesish, *members: Bytesish) -> int:
        """Remove members from the set; returns how many existed."""
        return await self._run(ops.srem(key, members))

    async def smembers(self, key: Bytesish) -> List[bytes]:
        """All members of the set (unordered); [] if absent."""
        return await self._run(ops.smembers(key))

    async def scard(self, key: Bytesish) -> int:
        """Number of members in the set; 0 if absent."""
        return await self._run(ops.scard(key))

    async def sismember(self, key: Bytesish, member: Bytesish) -> bool:
        """Whether ``member`` is in the set."""
        return await self._run(ops.sismember(key, member))

    async def sinter(self, *keys: Bytesish) -> List[bytes]:
        """Members present in every given set."""
        return await self._run(ops.sinter(keys))

    async def sunion(self, *keys: Bytesish) -> List[bytes]:
        """Members present in any of the given sets."""
        return await self._run(ops.sunion(keys))

    async def sdiff(self, *keys: Bytesish) -> List[bytes]:
        """Members in the first set but none of the rest."""
        return await self._run(ops.sdiff(keys))

    # --- sorted set (§3.5) ---------------------------------------------

    async def zadd(self, key: Bytesish, *members: ZArg) -> int:
        """Add scored members (``ZMember`` or ``(score, member)``); returns new members added."""
        return await self._run(ops.zadd(key, _norm_zmembers(members)))

    async def zrem(self, key: Bytesish, *members: Bytesish) -> int:
        """Remove members from the sorted set; returns how many existed."""
        return await self._run(ops.zrem(key, members))

    async def zscore(self, key: Bytesish, member: Bytesish) -> Optional[float]:
        """Score of ``member``, or None if absent."""
        return await self._run(ops.zscore(key, member))

    async def zcard(self, key: Bytesish) -> int:
        """Number of members in the sorted set; 0 if absent."""
        return await self._run(ops.zcard(key))

    async def zrange(self, key: Bytesish, start: int, stop: int) -> List[bytes]:
        """Members by rank in the inclusive [start, stop] range, low score first."""
        return await self._run(ops.zrange(key, start, stop))

    # --- sorted-set algebra (§3.6) -------------------------------------

    async def zinterstore(self, dest: Bytesish, *keys: Bytesish) -> int:
        """Store the intersection of the source sets at ``dest``; returns its cardinality."""
        return await self._run(ops.zinterstore(dest, keys))

    async def zinterstore_with(
        self,
        dest: Bytesish,
        keys: Sequence[Bytesish],
        weights: Optional[Sequence[float]] = None,
        aggregate: ZAggregate = ZAggregate.SUM,
    ) -> int:
        """ZINTERSTORE with per-key ``weights`` and an ``aggregate``; returns cardinality."""
        return await self._run(ops.zinterstore(dest, keys, weights, aggregate))

    async def zunionstore(self, dest: Bytesish, *keys: Bytesish) -> int:
        """Store the union of the source sets at ``dest``; returns its cardinality."""
        return await self._run(ops.zunionstore(dest, keys))

    async def zunionstore_with(
        self,
        dest: Bytesish,
        keys: Sequence[Bytesish],
        weights: Optional[Sequence[float]] = None,
        aggregate: ZAggregate = ZAggregate.SUM,
    ) -> int:
        """ZUNIONSTORE with per-key ``weights`` and an ``aggregate``; returns cardinality."""
        return await self._run(ops.zunionstore(dest, keys, weights, aggregate))

    async def zintercard(self, keys: Sequence[Bytesish], limit: Optional[int] = None) -> int:
        """Cardinality of the intersection, optionally capped at ``limit``."""
        return await self._run(ops.zintercard(keys, limit))

    # --- hash-field TTL (§3.7) -----------------------------------------

    async def hexpire(
        self,
        key: Bytesish,
        fields: Sequence[Bytesish],
        ttl: Duration,
        cond: HExpireCond = HExpireCond.ALWAYS,
    ) -> List[int]:
        """Set a per-field TTL (seconds); returns a status code per field."""
        return await self._run(ops.hexpire(key, fields, ttl, cond))

    async def hpexpire(
        self,
        key: Bytesish,
        fields: Sequence[Bytesish],
        ttl: Duration,
        cond: HExpireCond = HExpireCond.ALWAYS,
    ) -> List[int]:
        """Set a per-field TTL (milliseconds); returns a status code per field."""
        return await self._run(ops.hpexpire(key, fields, ttl, cond))

    async def hpersist(self, key: Bytesish, *fields: Bytesish) -> List[int]:
        """Clear per-field TTLs; returns a status code per field."""
        return await self._run(ops.hpersist(key, fields))

    async def httl(self, key: Bytesish, *fields: Bytesish) -> List[int]:
        """Remaining per-field TTL in seconds; a status/value per field."""
        return await self._run(ops.httl(key, fields))

    async def hpttl(self, key: Bytesish, *fields: Bytesish) -> List[int]:
        """Remaining per-field TTL in milliseconds; a status/value per field."""
        return await self._run(ops.hpttl(key, fields))

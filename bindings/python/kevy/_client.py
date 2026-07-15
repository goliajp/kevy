"""The unified blocking client (contract §1–§4).

One ``Client`` is opaque about whether it backs the in-process embedded
engine (``mem://`` / ``file://``) or a remote RESP server (``kevy://`` /
``redis://`` / ``tcp://``): a single ``connect(url)`` chooses, and the same
business code runs against either by changing only the URL. Errors are
raised as a ``KevyError`` hierarchy (§2). The asyncio twin is
:class:`kevy.AsyncClient`; both agree on results.
"""

from __future__ import annotations

from typing import List, Optional, Sequence, Tuple, Union

from . import _ops as ops
from ._decode import Bytesish, dec_bulks, to_bytes
from ._client_remote import RemoteMixin
from ._embedded import DB
from ._enums import HExpireCond, ZAggregate
from ._errors import InvalidInputError
from ._reply import Reply
from ._resp_conn import RespConn
from ._types import ZMember
from ._url import TargetKind, parse_connect_url, release_store, resolve_store

ZArg = Union[ZMember, Tuple[float, Bytesish]]


def _norm_zmembers(members: Sequence[ZArg]) -> List[Tuple[float, Bytesish]]:
    out: List[Tuple[float, Bytesish]] = []
    for m in members:
        if isinstance(m, ZMember):
            out.append((m.score, m.member))
        else:
            out.append((m[0], m[1]))
    return out


class Client(RemoteMixin):
    """An open connection to a kevy backend (§1.1)."""

    def __init__(self, remote: Optional[RespConn], emb: Optional[DB], emb_key: str, url: str):
        self._remote = remote
        self._emb = emb
        self._emb_key = emb_key
        self.url = url

    # --- lifecycle ------------------------------------------------------

    @classmethod
    def connect(cls, url: str) -> "Client":
        """Open a backend chosen by URL scheme (§1.1). Rejects TLS/AUTH and
        unknown schemes before any I/O; for kevy:// / redis:// with a /db it
        issues one SELECT round-trip."""
        target = parse_connect_url(url)
        if target.kind is TargetKind.REMOTE:
            conn = RespConn.dial(target.host, target.port)
            if target.db is not None:
                try:
                    conn.select_db(target.db)
                except Exception:
                    conn.close()
                    raise
            return cls(conn, None, "", url)
        db, key = resolve_store(target)
        return cls(None, db, key, url)

    def close(self) -> None:
        if self._remote is not None:
            self._remote.close()
            self._remote = None
        if self._emb is not None:
            release_store(self._emb_key, self._emb)
            self._emb = None

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    @property
    def is_embedded(self) -> bool:
        return self._emb is not None

    # --- transport ------------------------------------------------------

    def _exec(self, argv: List[bytes]) -> Reply:
        if self._emb is not None:
            return self._emb.cmd(*argv)
        if self._remote is not None:
            return self._remote.request(argv)
        from ._errors import ClosedError

        raise ClosedError()

    def _run(self, built) -> object:
        argv, dec = built
        return dec(self._exec(argv))

    # --- raw escape hatch (§7) -----------------------------------------

    def do(self, *argv: Bytesish) -> Reply:
        """Run any verb, returning the raw Reply so no server capability is
        unreachable. A -ERR reply is returned as a Reply of ERROR kind (data,
        not an exception), matching the pipeline inline-error convention."""
        if not argv:
            raise InvalidInputError("do needs at least a verb")
        return self._exec([to_bytes(a) for a in argv])

    cmd = do  # alias: the §5 embedded name

    # --- core string / generic (§3.1) ----------------------------------

    def ping(self) -> None:
        if self._emb is not None:
            return None
        self._run(ops.ping())

    def set(self, key: Bytesish, value: Bytesish) -> None:
        self._run(ops.set_(key, value))

    def get(self, key: Bytesish) -> Optional[bytes]:
        return self._run(ops.get(key))  # type: ignore[return-value]

    def delete(self, *keys: Bytesish) -> int:
        return self._run(ops.delete(keys))  # type: ignore[return-value]

    def exists(self, *keys: Bytesish) -> int:
        return self._run(ops.exists(keys))  # type: ignore[return-value]

    def incr(self, key: Bytesish) -> int:
        return self._run(ops.incr(key))  # type: ignore[return-value]

    def incr_by(self, key: Bytesish, delta: int) -> int:
        return self._run(ops.incr_by(key, delta))  # type: ignore[return-value]

    def expire(self, key: Bytesish, ttl) -> bool:
        return self._run(ops.expire(key, ttl))  # type: ignore[return-value]

    def persist(self, key: Bytesish) -> bool:
        return self._run(ops.persist(key))  # type: ignore[return-value]

    def ttl_ms(self, key: Bytesish) -> int:
        return self._run(ops.ttl_ms(key))  # type: ignore[return-value]

    def type_of(self, key: Bytesish) -> str:
        return self._run(ops.type_of(key))  # type: ignore[return-value]

    def dbsize(self) -> int:
        return self._run(ops.dbsize())  # type: ignore[return-value]

    def flushall(self) -> None:
        self._run(ops.flushall())

    def set_with_ttl(self, key: Bytesish, value: Bytesish, ttl) -> None:
        self._run(ops.set_with_ttl(key, value, ttl))

    def mget(self, *keys: Bytesish) -> List[Optional[bytes]]:
        return self._run(ops.mget(keys))  # type: ignore[return-value]

    def mset(self, *pairs: Bytesish) -> None:
        self._run(ops.mset(pairs))

    def publish(self, channel: Bytesish, message: Bytesish) -> int:
        return self._run(ops.publish(channel, message))  # type: ignore[return-value]

    # --- hash (§3.2) ----------------------------------------------------

    def hset(self, key: Bytesish, *pairs: Bytesish) -> int:
        return self._run(ops.hset(key, pairs))  # type: ignore[return-value]

    def hget(self, key: Bytesish, field: Bytesish) -> Optional[bytes]:
        return self._run(ops.hget(key, field))  # type: ignore[return-value]

    def hdel(self, key: Bytesish, *fields: Bytesish) -> int:
        return self._run(ops.hdel(key, fields))  # type: ignore[return-value]

    def hlen(self, key: Bytesish) -> int:
        return self._run(ops.hlen(key))  # type: ignore[return-value]

    def hgetall(self, key: Bytesish) -> List[Optional[bytes]]:
        return self._run(ops.hgetall(key))  # type: ignore[return-value]

    def hkeys(self, key: Bytesish) -> List[Optional[bytes]]:
        return self._run(ops.hkeys(key))  # type: ignore[return-value]

    def hvals(self, key: Bytesish) -> List[Optional[bytes]]:
        return self._run(ops.hvals(key))  # type: ignore[return-value]

    # --- list (§3.3) ----------------------------------------------------

    def lpush(self, key: Bytesish, *values: Bytesish) -> int:
        return self._run(ops.lpush(key, values))  # type: ignore[return-value]

    def rpush(self, key: Bytesish, *values: Bytesish) -> int:
        return self._run(ops.rpush(key, values))  # type: ignore[return-value]

    def lpop(self, key: Bytesish, count: int = 1) -> List[bytes]:
        return self._run(ops.lpop(key, count))  # type: ignore[return-value]

    def rpop(self, key: Bytesish, count: int = 1) -> List[bytes]:
        return self._run(ops.rpop(key, count))  # type: ignore[return-value]

    def llen(self, key: Bytesish) -> int:
        return self._run(ops.llen(key))  # type: ignore[return-value]

    def lrange(self, key: Bytesish, start: int, stop: int) -> List[bytes]:
        return self._run(ops.lrange(key, start, stop))  # type: ignore[return-value]

    # --- set (§3.4) -----------------------------------------------------

    def sadd(self, key: Bytesish, *members: Bytesish) -> int:
        return self._run(ops.sadd(key, members))  # type: ignore[return-value]

    def srem(self, key: Bytesish, *members: Bytesish) -> int:
        return self._run(ops.srem(key, members))  # type: ignore[return-value]

    def smembers(self, key: Bytesish) -> List[bytes]:
        return self._run(ops.smembers(key))  # type: ignore[return-value]

    def scard(self, key: Bytesish) -> int:
        return self._run(ops.scard(key))  # type: ignore[return-value]

    def sismember(self, key: Bytesish, member: Bytesish) -> bool:
        return self._run(ops.sismember(key, member))  # type: ignore[return-value]

    def sinter(self, *keys: Bytesish) -> List[bytes]:
        return self._run(ops.sinter(keys))  # type: ignore[return-value]

    def sunion(self, *keys: Bytesish) -> List[bytes]:
        return self._run(ops.sunion(keys))  # type: ignore[return-value]

    def sdiff(self, *keys: Bytesish) -> List[bytes]:
        return self._run(ops.sdiff(keys))  # type: ignore[return-value]

    # --- sorted set (§3.5) ---------------------------------------------

    def zadd(self, key: Bytesish, *members: ZArg) -> int:
        return self._run(ops.zadd(key, _norm_zmembers(members)))  # type: ignore[return-value]

    def zrem(self, key: Bytesish, *members: Bytesish) -> int:
        return self._run(ops.zrem(key, members))  # type: ignore[return-value]

    def zscore(self, key: Bytesish, member: Bytesish) -> Optional[float]:
        return self._run(ops.zscore(key, member))  # type: ignore[return-value]

    def zcard(self, key: Bytesish) -> int:
        return self._run(ops.zcard(key))  # type: ignore[return-value]

    def zrange(self, key: Bytesish, start: int, stop: int) -> List[bytes]:
        return self._run(ops.zrange(key, start, stop))  # type: ignore[return-value]

    # --- sorted-set algebra (§3.6) -------------------------------------

    def zinterstore(self, dest: Bytesish, *keys: Bytesish) -> int:
        return self._run(ops.zinterstore(dest, keys))  # type: ignore[return-value]

    def zinterstore_with(self, dest, keys, weights=None, aggregate=ZAggregate.SUM) -> int:
        return self._run(ops.zinterstore(dest, keys, weights, aggregate))  # type: ignore[return-value]

    def zunionstore(self, dest: Bytesish, *keys: Bytesish) -> int:
        return self._run(ops.zunionstore(dest, keys))  # type: ignore[return-value]

    def zunionstore_with(self, dest, keys, weights=None, aggregate=ZAggregate.SUM) -> int:
        return self._run(ops.zunionstore(dest, keys, weights, aggregate))  # type: ignore[return-value]

    def zintercard(self, keys, limit: Optional[int] = None) -> int:
        return self._run(ops.zintercard(keys, limit))  # type: ignore[return-value]

    # --- hash-field TTL (§3.7) -----------------------------------------

    def hexpire(self, key, fields, ttl, cond=HExpireCond.ALWAYS) -> List[int]:
        return self._run(ops.hexpire(key, fields, ttl, cond))  # type: ignore[return-value]

    def hpexpire(self, key, fields, ttl, cond=HExpireCond.ALWAYS) -> List[int]:
        return self._run(ops.hpexpire(key, fields, ttl, cond))  # type: ignore[return-value]

    def hpersist(self, key, *fields: Bytesish) -> List[int]:
        return self._run(ops.hpersist(key, fields))  # type: ignore[return-value]

    def httl(self, key, *fields: Bytesish) -> List[int]:
        return self._run(ops.httl(key, fields))  # type: ignore[return-value]

    def hpttl(self, key, *fields: Bytesish) -> List[int]:
        return self._run(ops.hpttl(key, fields))  # type: ignore[return-value]

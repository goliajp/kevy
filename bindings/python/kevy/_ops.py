"""Command builders shared by both faces (contract §3.1–§3.7).

Each builder returns ``(argv, decode)`` — the wire argv (a list of bytes)
and the pure decoder for its reply. Input validation that must happen
*before* any I/O (even/odd arities, empty source keys, empty fields) raises
here. The blocking ``Client`` and the ``AsyncClient`` both call these, so
the two faces agree on argv, decoding, and pre-flight errors. Backend-aware
commands (ping short-circuit, blocking pops, remote-only families) live in
the client classes, not here.
"""

from __future__ import annotations

from datetime import timedelta
from typing import Callable, List, Optional, Sequence, Tuple

from . import _decode as d
from ._decode import Bytesish, to_bytes
from ._enums import HExpireCond, ZAggregate
from ._errors import InvalidInputError
from ._reply import Reply

Argv = List[bytes]
Built = Tuple[Argv, Callable[[Reply], object]]

_I64_MAX = 9223372036854775807


def _f(x: float) -> bytes:
    return repr(float(x)).encode("ascii")


def to_ms(ttl) -> int:
    """Duration → milliseconds, clamped to i64::MAX (§3.1). Accepts a
    ``timedelta`` or a number of seconds."""
    if isinstance(ttl, timedelta):
        ms = int(ttl.total_seconds() * 1000)
    else:
        ms = int(float(ttl) * 1000)
    if ms < 0:
        return 0
    return min(ms, _I64_MAX)


def to_secs(ttl) -> int:
    """Duration → whole seconds, truncated (§3.7 HEXPIRE precision)."""
    if isinstance(ttl, timedelta):
        secs = int(ttl.total_seconds())
    else:
        secs = int(float(ttl))
    return max(secs, 0)


def seconds_of(ttl) -> float:
    if isinstance(ttl, timedelta):
        return ttl.total_seconds()
    return float(ttl)


def _bkeys(keys: Sequence[Bytesish]) -> Argv:
    return [to_bytes(k) for k in keys]


# --- core string / generic (§3.1) --------------------------------------


def ping() -> Built:
    return [b"PING"], d.dec_pong


def set_(key: Bytesish, value: Bytesish) -> Built:
    return [b"SET", to_bytes(key), to_bytes(value)], d.dec_ok


def get(key: Bytesish) -> Built:
    return [b"GET", to_bytes(key)], d.dec_opt_bulk


def delete(keys: Sequence[Bytesish]) -> Built:
    return [b"DEL", *_bkeys(keys)], d.dec_count


def exists(keys: Sequence[Bytesish]) -> Built:
    return [b"EXISTS", *_bkeys(keys)], d.dec_count


def incr(key: Bytesish) -> Built:
    return [b"INCR", to_bytes(key)], d.dec_int


def incr_by(key: Bytesish, delta: int) -> Built:
    return [b"INCRBY", to_bytes(key), str(int(delta)).encode()], d.dec_int


def expire(key: Bytesish, ttl) -> Built:
    return [b"PEXPIRE", to_bytes(key), str(to_ms(ttl)).encode()], d.dec_bool


def persist(key: Bytesish) -> Built:
    return [b"PERSIST", to_bytes(key)], d.dec_bool


def ttl_ms(key: Bytesish) -> Built:
    return [b"PTTL", to_bytes(key)], d.dec_int


def type_of(key: Bytesish) -> Built:
    return [b"TYPE", to_bytes(key)], d.dec_str_simple


def dbsize() -> Built:
    return [b"DBSIZE"], d.dec_count


def flushall() -> Built:
    return [b"FLUSHALL"], d.dec_ok


def set_with_ttl(key: Bytesish, value: Bytesish, ttl) -> Built:
    return [b"SET", to_bytes(key), to_bytes(value), b"PX", str(to_ms(ttl)).encode()], d.dec_ok


def mget(keys: Sequence[Bytesish]) -> Built:
    return [b"MGET", *_bkeys(keys)], d.dec_bulks


def mset(pairs: Sequence[Bytesish]) -> Built:
    argv = _bkeys(pairs)
    if len(argv) % 2 != 0:
        raise InvalidInputError("mset needs an even number of key/value arguments")
    return [b"MSET", *argv], d.dec_ok


def publish(channel: Bytesish, message: Bytesish) -> Built:
    return [b"PUBLISH", to_bytes(channel), to_bytes(message)], d.dec_count


# --- hash (§3.2) --------------------------------------------------------


def hset(key: Bytesish, pairs: Sequence[Bytesish]) -> Built:
    argv = _bkeys(pairs)
    if len(argv) % 2 != 0:
        raise InvalidInputError("hset needs an even number of field/value arguments")
    return [b"HSET", to_bytes(key), *argv], d.dec_count


def hget(key: Bytesish, field: Bytesish) -> Built:
    return [b"HGET", to_bytes(key), to_bytes(field)], d.dec_opt_bulk


def hdel(key: Bytesish, fields: Sequence[Bytesish]) -> Built:
    return [b"HDEL", to_bytes(key), *_bkeys(fields)], d.dec_count


def hlen(key: Bytesish) -> Built:
    return [b"HLEN", to_bytes(key)], d.dec_count


def hgetall(key: Bytesish) -> Built:
    return [b"HGETALL", to_bytes(key)], d.dec_bulks


def hkeys(key: Bytesish) -> Built:
    return [b"HKEYS", to_bytes(key)], d.dec_bulks


def hvals(key: Bytesish) -> Built:
    return [b"HVALS", to_bytes(key)], d.dec_bulks


# --- list (§3.3) --------------------------------------------------------


def lpush(key: Bytesish, values: Sequence[Bytesish]) -> Built:
    return [b"LPUSH", to_bytes(key), *_bkeys(values)], d.dec_count


def rpush(key: Bytesish, values: Sequence[Bytesish]) -> Built:
    return [b"RPUSH", to_bytes(key), *_bkeys(values)], d.dec_count


def lpop(key: Bytesish, count: int) -> Built:
    return [b"LPOP", to_bytes(key), str(int(count)).encode()], d.dec_list_pop


def rpop(key: Bytesish, count: int) -> Built:
    return [b"RPOP", to_bytes(key), str(int(count)).encode()], d.dec_list_pop


def llen(key: Bytesish) -> Built:
    return [b"LLEN", to_bytes(key)], d.dec_count


def lrange(key: Bytesish, start: int, stop: int) -> Built:
    return [b"LRANGE", to_bytes(key), str(int(start)).encode(), str(int(stop)).encode()], d.dec_bulks


# --- set (§3.4) ---------------------------------------------------------


def sadd(key: Bytesish, members: Sequence[Bytesish]) -> Built:
    return [b"SADD", to_bytes(key), *_bkeys(members)], d.dec_count


def srem(key: Bytesish, members: Sequence[Bytesish]) -> Built:
    return [b"SREM", to_bytes(key), *_bkeys(members)], d.dec_count


def smembers(key: Bytesish) -> Built:
    return [b"SMEMBERS", to_bytes(key)], d.dec_bulks


def scard(key: Bytesish) -> Built:
    return [b"SCARD", to_bytes(key)], d.dec_count


def sismember(key: Bytesish, member: Bytesish) -> Built:
    return [b"SISMEMBER", to_bytes(key), to_bytes(member)], d.dec_bool


def sinter(keys: Sequence[Bytesish]) -> Built:
    return [b"SINTER", *_bkeys(keys)], d.dec_bulks


def sunion(keys: Sequence[Bytesish]) -> Built:
    return [b"SUNION", *_bkeys(keys)], d.dec_bulks


def sdiff(keys: Sequence[Bytesish]) -> Built:
    return [b"SDIFF", *_bkeys(keys)], d.dec_bulks


# --- sorted set (§3.5) --------------------------------------------------


def zadd(key: Bytesish, scored: Sequence[Tuple[float, Bytesish]]) -> Built:
    argv: Argv = [b"ZADD", to_bytes(key)]
    for score, member in scored:
        argv.append(_f(score))
        argv.append(to_bytes(member))
    return argv, d.dec_count


def zrem(key: Bytesish, members: Sequence[Bytesish]) -> Built:
    return [b"ZREM", to_bytes(key), *_bkeys(members)], d.dec_count


def zscore(key: Bytesish, member: Bytesish) -> Built:
    return [b"ZSCORE", to_bytes(key), to_bytes(member)], d.dec_opt_float


def zcard(key: Bytesish) -> Built:
    return [b"ZCARD", to_bytes(key)], d.dec_count


def zrange(key: Bytesish, start: int, stop: int) -> Built:
    return [b"ZRANGE", to_bytes(key), str(int(start)).encode(), str(int(stop)).encode()], d.dec_bulks


# --- sorted-set algebra (§3.6) ------------------------------------------


def _check_source(keys: Sequence[Bytesish]) -> Argv:
    argv = _bkeys(keys)
    if not argv:
        raise InvalidInputError("zset algebra needs at least one source key")
    return argv


def _zstore(verb: bytes, dest: Bytesish, keys, weights, agg: ZAggregate) -> Built:
    ks = _check_source(keys)
    if weights is not None and len(weights) != len(ks):
        raise InvalidInputError("weights must be one per source key")
    argv: Argv = [verb, to_bytes(dest), str(len(ks)).encode(), *ks]
    if weights is not None:
        argv.append(b"WEIGHTS")
        argv.extend(_f(w) for w in weights)
    if agg is not ZAggregate.SUM:
        argv.extend([b"AGGREGATE", agg.tag()])
    return argv, d.dec_count


def zinterstore(dest, keys, weights=None, agg=ZAggregate.SUM) -> Built:
    return _zstore(b"ZINTERSTORE", dest, keys, weights, agg)


def zunionstore(dest, keys, weights=None, agg=ZAggregate.SUM) -> Built:
    return _zstore(b"ZUNIONSTORE", dest, keys, weights, agg)


def zintercard(keys, limit: Optional[int]) -> Built:
    ks = _check_source(keys)
    argv: Argv = [b"ZINTERCARD", str(len(ks)).encode(), *ks]
    if limit is not None:
        argv.extend([b"LIMIT", str(int(limit)).encode()])
    return argv, d.dec_count


# --- hash-field TTL (§3.7) ----------------------------------------------


def _check_fields(fields: Sequence[Bytesish]) -> Argv:
    argv = _bkeys(fields)
    if not argv:
        raise InvalidInputError("hash field-TTL verbs need at least one field")
    return argv


def _httl_set(verb: bytes, key, arg: bytes, cond: HExpireCond, fields) -> Built:
    fs = _check_fields(fields)
    argv: Argv = [verb, to_bytes(key), arg]
    kw = cond.keyword()
    if kw is not None:
        argv.append(kw)
    argv.extend([b"FIELDS", str(len(fs)).encode(), *fs])
    return argv, d.dec_int_list


def hexpire(key, fields, ttl, cond=HExpireCond.ALWAYS) -> Built:
    return _httl_set(b"HEXPIRE", key, str(to_secs(ttl)).encode(), cond, fields)


def hpexpire(key, fields, ttl, cond=HExpireCond.ALWAYS) -> Built:
    return _httl_set(b"HPEXPIRE", key, str(to_ms(ttl)).encode(), cond, fields)


def hpersist(key, fields) -> Built:
    fs = _check_fields(fields)
    return [b"HPERSIST", to_bytes(key), b"FIELDS", str(len(fs)).encode(), *fs], d.dec_int_list


def httl(key, fields) -> Built:
    fs = _check_fields(fields)
    return [b"HTTL", to_bytes(key), b"FIELDS", str(len(fs)).encode(), *fs], d.dec_int_list


def hpttl(key, fields) -> Built:
    fs = _check_fields(fields)
    return [b"HPTTL", to_bytes(key), b"FIELDS", str(len(fs)).encode(), *fs], d.dec_int_list

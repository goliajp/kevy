"""Remote-facing and backend-aware families for the blocking client:
declarative indexes (§3.8), change feed (§3.10), blocking pops (§3.14), and
the transaction / pipeline entry points (§3.12–§3.13).

Provided as a mixin so :class:`kevy.Client` stays focused on the pure
families. Methods here consult the backend: IDX.* / MULTI / pipeline are
remote-only (embedded → ``UnsupportedError``); blocking pops run for real on
remote and are *emulated by polling* on the embedded C-ABI backend (§3.14).
"""

from __future__ import annotations

import time
from typing import List, Optional, Sequence, Tuple

from ._decode import Bytesish, dec_bulks, to_bytes
from ._enums import IdxType
from ._errors import (
    InvalidInputError,
    ProtocolError,
    UnsupportedError,
    error_from_reply_text,
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
from ._transaction import Transaction
from ._types import FeedBatch, IdxInfo, IdxPage, KV, Ranked, ZPopHit


class RemoteMixin:
    # These attributes are supplied by Client; declared for type checkers.
    _remote: object
    _emb: object

    def _require_remote(self, feature: str):
        if getattr(self, "_remote", None) is not None:
            return self._remote
        raise UnsupportedError(
            f"{feature} is remote-only; on the embedded backend use the raw cmd() path"
        )

    def _exec(self, argv: List[bytes]) -> Reply:  # provided by Client
        raise NotImplementedError

    # --- declarative indexes: IDX.* (§3.8) -----------------------------

    def idx_create_range(self, name: Bytesish, prefix: Bytesish, field: Bytesish, ty: IdxType) -> None:
        self.idx_create_raw(
            to_bytes(name), b"ON", b"PREFIX", to_bytes(prefix),
            b"FIELD", to_bytes(field), b"TYPE", ty.tag(), b"KIND", b"range",
        )

    def idx_create_raw(self, *args: Bytesish) -> None:
        self._require_remote("IDX.CREATE")
        r = self._exec([b"IDX.CREATE", *[to_bytes(a) for a in args]])
        _expect_ok(r)

    def idx_drop(self, name: Bytesish) -> bool:
        self._require_remote("IDX.DROP")
        r = self._exec([b"IDX.DROP", to_bytes(name)])
        if r.kind is ReplyKind.INT:
            return r.integer == 1
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"IDX.DROP: unexpected {r.shape()}")

    def idx_list(self) -> List[IdxInfo]:
        self._require_remote("IDX.LIST")
        return parse_idx_list(self._exec([b"IDX.LIST"]))

    def idx_query_range(self, name: Bytesish, min_: Bytesish, max_: Bytesish, limit: int,
                        cursor: Optional[bytes] = None) -> IdxPage:
        args = [b"RANGE", to_bytes(min_), to_bytes(max_), b"LIMIT", str(int(limit)).encode()]
        if cursor is not None:
            args += [b"CURSOR", to_bytes(cursor)]
        return parse_idx_page(self._idx_query(name, args))

    def idx_query_eq(self, name: Bytesish, value: Bytesish, limit: int) -> IdxPage:
        args = [b"EQ", to_bytes(value), b"LIMIT", str(int(limit)).encode()]
        return parse_idx_page(self._idx_query(name, args))

    def idx_query_match(self, name: Bytesish, text: Bytesish, limit: int) -> List[Ranked]:
        args = [b"MATCH", to_bytes(text), b"LIMIT", str(int(limit)).encode()]
        return parse_ranked(self._idx_query(name, args))

    def idx_query_knn(self, name: Bytesish, vector: Sequence[float], k: int) -> List[Ranked]:
        args = [b"KNN", knn_blob(list(vector)), b"LIMIT", str(int(k)).encode()]
        return parse_ranked(self._idx_query(name, args))

    def idx_query_raw(self, *args: Bytesish) -> Reply:
        self._require_remote("IDX.QUERY")
        r = self._exec([b"IDX.QUERY", *[to_bytes(a) for a in args]])
        if r.is_error():
            raise error_from_reply_text(r.data)
        return r

    def _idx_query(self, name: Bytesish, args: List[bytes]) -> Reply:
        return self.idx_query_raw(to_bytes(name), *args)

    # --- change feed: FEED.* (§3.10) -----------------------------------

    def feed_shards(self) -> int:
        if getattr(self, "_emb", None) is not None:
            return 1
        r = self._exec([b"FEED.SHARDS"])
        return _expect_count(r)

    def feed_tail(self, shard: int) -> Tuple[int, int]:
        if getattr(self, "_emb", None) is not None:
            _embedded_feed_guard(shard)
        r = self._exec([b"FEED.TAIL", str(int(shard)).encode()])
        if r.kind is ReplyKind.ARRAY and len(r.items) == 2 and all(
            it.kind is ReplyKind.INT for it in r.items
        ):
            return r.items[0].integer, r.items[1].integer
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"FEED.TAIL: unexpected {r.shape()}")

    def feed_read(self, shard: int, generation: int, offset: int,
                  count: Optional[int] = None, prefixes: Sequence[Bytesish] = ()) -> FeedBatch:
        if getattr(self, "_emb", None) is not None:
            _embedded_feed_guard(shard)
        argv = [b"FEED.READ", str(int(shard)).encode(), str(int(generation)).encode(), str(int(offset)).encode()]
        if count is not None and count > 0:
            argv += [b"COUNT", str(int(count)).encode()]
        for p in prefixes:
            argv += [b"PREFIX", to_bytes(p)]
        return parse_feed_batch(self._exec(argv))

    # --- blocking pops (§3.14) -----------------------------------------

    def blpop(self, keys: Sequence[Bytesish], timeout=None) -> Optional[KV]:
        kb = _check_block(keys, timeout)
        if getattr(self, "_emb", None) is not None:
            return self._emb_block_kv(b"LPOP", kb, timeout)
        return self._pop_kv(b"BLPOP", kb, timeout)

    def brpop(self, keys: Sequence[Bytesish], timeout=None) -> Optional[KV]:
        kb = _check_block(keys, timeout)
        if getattr(self, "_emb", None) is not None:
            return self._emb_block_kv(b"RPOP", kb, timeout)
        return self._pop_kv(b"BRPOP", kb, timeout)

    def bzpopmin(self, keys: Sequence[Bytesish], timeout=None) -> Optional[ZPopHit]:
        kb = _check_block(keys, timeout)
        if getattr(self, "_emb", None) is not None:
            return self._emb_block_z(kb, timeout)
        r = self._exec(_block_argv(b"BZPOPMIN", kb, timeout))
        if r.kind is ReplyKind.ARRAY and len(r.items) == 3:
            k, m, s = r.items
            if k.kind is not ReplyKind.BULK or m.kind is not ReplyKind.BULK:
                raise ProtocolError("BZPOPMIN: bad hit shape")
            from ._decode import score_of
            return ZPopHit(k.data, m.data, score_of(s))
        if r.is_nil():
            return None
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"BZPOPMIN: unexpected {r.shape()}")

    def _pop_kv(self, verb: bytes, keys: List[bytes], timeout) -> Optional[KV]:
        r = self._exec(_block_argv(verb, keys, timeout))
        if r.kind is ReplyKind.ARRAY and len(r.items) == 2:
            k, v = r.items
            if k.kind is not ReplyKind.BULK or v.kind is not ReplyKind.BULK:
                raise ProtocolError(f"{verb!r}: bad hit shape")
            return KV(k.data, v.data)
        if r.is_nil():
            return None
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"{verb!r}: unexpected {r.shape()}")

    def _emb_block_kv(self, verb: bytes, keys: List[bytes], timeout) -> Optional[KV]:
        deadline = None if timeout is None else time.monotonic() + seconds_of(timeout)
        while True:
            for k in keys:
                r = self._exec([verb, k, b"1"])
                if r.kind is ReplyKind.ARRAY and r.items and r.items[0].kind is ReplyKind.BULK:
                    return KV(k, r.items[0].data)
                if r.kind is ReplyKind.BULK:
                    return KV(k, r.data)
            if _block_done(deadline):
                return None

    def _emb_block_z(self, keys: List[bytes], timeout) -> Optional[ZPopHit]:
        from ._decode import score_of

        deadline = None if timeout is None else time.monotonic() + seconds_of(timeout)
        while True:
            for k in keys:
                r = self._exec([b"ZPOPMIN", k, b"1"])
                if r.kind is ReplyKind.ARRAY and len(r.items) >= 2 and r.items[0].kind is ReplyKind.BULK:
                    return ZPopHit(k, r.items[0].data, score_of(r.items[1]))
            if _block_done(deadline):
                return None

    # --- transactions & pipeline entry (§3.12–§3.13) -------------------

    def watch(self, *keys: Bytesish) -> None:
        if not keys:
            raise InvalidInputError("watch needs at least one key")
        conn = self._require_remote("WATCH")
        _expect_ok(conn.request([b"WATCH", *[to_bytes(k) for k in keys]]))

    def unwatch(self) -> None:
        conn = self._require_remote("UNWATCH")
        _expect_ok(conn.request([b"UNWATCH"]))

    def multi(self) -> Transaction:
        conn = self._require_remote("MULTI/EXEC")
        _expect_ok(conn.request([b"MULTI"]))
        return Transaction(conn)

    def pipeline(self, build) -> List[Reply]:
        conn = self._require_remote("pipeline")
        buf = PipelineBuf()
        build(buf)
        if buf.poisoned:
            raise InvalidInputError("pipeline: a queued command had an empty argv")
        if buf.length == 0:
            return []
        return conn.pipeline_raw(buf.wire, buf.length)


# --- shared helpers -----------------------------------------------------


def _expect_ok(r: Reply) -> None:
    if r.kind is ReplyKind.SIMPLE and r.data == b"OK":
        return
    if r.is_error():
        raise error_from_reply_text(r.data)
    raise ProtocolError(f"expected +OK, got {r.shape()}")


def _expect_count(r: Reply) -> int:
    if r.kind is ReplyKind.INT:
        if r.integer < 0:
            raise ProtocolError("expected non-negative count")
        return r.integer
    if r.is_error():
        raise error_from_reply_text(r.data)
    raise ProtocolError(f"expected integer, got {r.shape()}")


def _embedded_feed_guard(shard: int) -> None:
    if shard != 0:
        raise InvalidInputError("embedded feed is single-shard: shard must be 0")
    raise UnsupportedError(
        "feed disabled: this port's embedded connect does not enable the change feed"
    )


def _check_block(keys: Sequence[Bytesish], timeout) -> List[bytes]:
    kb = [to_bytes(k) for k in keys]
    if not kb:
        raise InvalidInputError("blocking pop needs at least one key")
    if timeout is not None and seconds_of(timeout) == 0:
        raise InvalidInputError(
            "timeout of zero is ambiguous (wire 0 = wait forever); use None to wait forever"
        )
    return kb


def _block_argv(verb: bytes, keys: List[bytes], timeout) -> List[bytes]:
    argv = [verb, *keys]
    if timeout is None:
        argv.append(b"0")
    else:
        argv.append(repr(float(seconds_of(timeout))).encode("ascii"))
    return argv


def _block_done(deadline: Optional[float]) -> bool:
    if deadline is not None and time.monotonic() >= deadline:
        return True
    time.sleep(0.005)
    return False

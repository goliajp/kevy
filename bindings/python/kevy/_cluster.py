"""Cluster-aware client (contract §3.15) + CRC16 slot routing (§7).

One connection per shard, CRC16-slot routed so single-key commands hit their
owner shard directly (no server ``-MOVED`` hop). Requires the server in
cluster mode; topology is discovered once at connect via ``CLUSTER SLOTS``
(16384 slots). The CRC16 reproduces Redis's ``key_hash_slot`` exactly
(including ``{hashtag}`` extraction) so routing agrees with the server.
"""

from __future__ import annotations

from typing import List, Optional, Tuple

from ._decode import Bytesish, score_of, to_bytes
from ._errors import ProtocolError, error_from_reply_text
from ._ops import to_ms
from ._reply import Reply, ReplyKind
from ._resp_conn import RespConn

_NUM_SLOTS = 16384
_CRC_POLY = 0x1021


def _make_crc_table() -> List[int]:
    table = []
    for i in range(256):
        crc = i << 8
        for _ in range(8):
            crc = ((crc << 1) ^ _CRC_POLY) & 0xFFFF if (crc & 0x8000) else (crc << 1) & 0xFFFF
        table.append(crc)
    return table


_CRC_TABLE = _make_crc_table()


def crc16(data: bytes) -> int:
    crc = 0
    for c in data:
        crc = ((crc << 8) ^ _CRC_TABLE[((crc >> 8) ^ c) & 0xFF]) & 0xFFFF
    return crc


def key_hash_slot(key: bytes) -> int:
    """Redis-cluster hash slot of key: crc16(hashtag(key)) & 16383."""
    return crc16(_hashtag(key)) & 0x3FFF


def _hashtag(key: bytes) -> bytes:
    open_i = key.find(b"{")
    if open_i < 0:
        return key
    close_i = key.find(b"}", open_i + 1)
    if close_i < 0:
        return key
    if close_i == open_i + 1:  # empty {} → hash whole key
        return key
    return key[open_i + 1 : close_i]


class ClusterClient:
    def __init__(self, shards: List[RespConn], slot_to_shard: List[int]):
        self._shards = shards
        self._slot_to_shard = slot_to_shard

    @classmethod
    def connect(cls, host: str, port: int) -> "ClusterClient":
        seed = RespConn.dial(host, port)
        try:
            reply = seed.request([b"CLUSTER", b"SLOTS"])
        finally:
            seed.close()
        ranges = _parse_cluster_slots(reply)
        nodes, table = _build_topology(ranges)
        shards: List[RespConn] = []
        try:
            for h, p in nodes:
                shards.append(RespConn.dial(h, p))
        except Exception:
            for s in shards:
                s.close()
            raise
        return cls(shards, table)

    def close(self) -> None:
        for s in self._shards:
            s.close()
        self._shards = []

    def __enter__(self) -> "ClusterClient":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    @property
    def shard_count(self) -> int:
        return len(self._shards)

    def _shard_for(self, key: bytes) -> RespConn:
        return self._shards[self._slot_to_shard[key_hash_slot(key)]]

    def request_keyed(self, key: Bytesish, *argv: Bytesish) -> Reply:
        return self._shard_for(to_bytes(key)).request([to_bytes(a) for a in argv])

    def request_unkeyed(self, *argv: Bytesish) -> Reply:
        return self._shards[0].request([to_bytes(a) for a in argv])

    def ping(self) -> None:
        r = self.request_unkeyed(b"PING")
        if r.kind is ReplyKind.SIMPLE and r.data in (b"PONG", b"OK"):
            return
        _raise_reply(r)

    def publish(self, channel: Bytesish, message: Bytesish) -> int:
        return _count(self.request_unkeyed(b"PUBLISH", channel, message))

    def set(self, key: Bytesish, value: Bytesish) -> None:
        _ok(self.request_keyed(key, b"SET", key, value))

    def set_with_ttl(self, key: Bytesish, value: Bytesish, ttl) -> None:
        _ok(self.request_keyed(key, b"SET", key, value, b"PX", str(to_ms(ttl)).encode()))

    def get(self, key: Bytesish) -> Optional[bytes]:
        r = self.request_keyed(key, b"GET", key)
        if r.kind is ReplyKind.BULK:
            return r.data
        if r.is_nil():
            return None
        _raise_reply(r)

    def incr(self, key: Bytesish) -> int:
        return _int(self.request_keyed(key, b"INCR", key))

    def incr_by(self, key: Bytesish, delta: int) -> int:
        return _int(self.request_keyed(key, b"INCRBY", key, str(int(delta)).encode()))

    def expire(self, key: Bytesish, ttl) -> bool:
        return _bool(self.request_keyed(key, b"PEXPIRE", key, str(to_ms(ttl)).encode()))

    def persist(self, key: Bytesish) -> bool:
        return _bool(self.request_keyed(key, b"PERSIST", key))

    def ttl_ms(self, key: Bytesish) -> int:
        return _int(self.request_keyed(key, b"PTTL", key))

    def delete(self, *keys: Bytesish) -> int:
        return self._per_key_sum(b"DEL", keys)

    def exists(self, *keys: Bytesish) -> int:
        return self._per_key_sum(b"EXISTS", keys)

    def _per_key_sum(self, verb: bytes, keys) -> int:
        total = 0
        for k in keys:
            n = _int(self.request_keyed(k, verb, k))
            if n > 0:
                total += n
        return total

    def dbsize(self) -> int:
        return _count(self.request_unkeyed(b"DBSIZE"))

    def flushall(self) -> None:
        _ok(self.request_unkeyed(b"FLUSHALL"))


# --- CLUSTER SLOTS parsing + topology -----------------------------------


def _build_topology(ranges: List[Tuple[int, int, str, int]]):
    if not ranges:
        raise ProtocolError("CLUSTER SLOTS returned no ranges")
    nodes: List[Tuple[str, int]] = []
    table = [0] * _NUM_SLOTS
    for start, end, host, port in ranges:
        node = (host, port)
        if node in nodes:
            idx = nodes.index(node)
        else:
            nodes.append(node)
            idx = len(nodes) - 1
        for slot in range(start, end + 1):
            table[slot] = idx
    return nodes, table


def _parse_cluster_slots(reply: Reply) -> List[Tuple[int, int, str, int]]:
    def bad():
        return ProtocolError("malformed CLUSTER SLOTS reply")

    if reply.kind is not ReplyKind.ARRAY:
        raise bad()
    out: List[Tuple[int, int, str, int]] = []
    for row in reply.items:
        if row.kind is not ReplyKind.ARRAY or len(row.items) < 3:
            raise bad()
        start = _as_int(row.items[0])
        end = _as_int(row.items[1])
        node = row.items[2]
        if start is None or end is None or node.kind is not ReplyKind.ARRAY or len(node.items) < 2:
            raise bad()
        host = _as_str(node.items[0])
        port = _as_int(node.items[1])
        if host is None or port is None:
            raise bad()
        out.append((start, end, host, port))
    return out


def _as_int(r: Reply) -> Optional[int]:
    return r.integer if r.kind is ReplyKind.INT else None


def _as_str(r: Reply) -> Optional[str]:
    if r.kind in (ReplyKind.BULK, ReplyKind.SIMPLE):
        return r.text()
    return None


def _raise_reply(r: Reply):
    if r.is_error():
        raise error_from_reply_text(r.data)
    raise ProtocolError(f"unexpected reply variant: {r.shape()}")


def _int(r: Reply) -> int:
    if r.kind is ReplyKind.INT:
        return r.integer
    _raise_reply(r)


def _count(r: Reply) -> int:
    n = _int(r)
    if n < 0:
        raise ProtocolError(f"expected non-negative count, got {n}")
    return n


def _bool(r: Reply) -> bool:
    if r.kind is ReplyKind.INT:
        return r.integer == 1
    _raise_reply(r)


def _ok(r: Reply) -> None:
    if r.kind is ReplyKind.SIMPLE and r.data == b"OK":
        return
    _raise_reply(r)

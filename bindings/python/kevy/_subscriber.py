"""Pub/sub consumer side (contract §3.11).

A subscribed connection cannot send normal commands, so the consumer is a
distinct ``Subscriber`` type; publishing is done from a normal ``Client``
via ``publish``. Remote URLs use a dedicated TCP socket; ``mem://<name>`` /
``file://`` use the in-process bus (via the C-ABI subscription handles).
Anonymous ``mem://`` is rejected — no other producer.
"""

from __future__ import annotations

import time
from typing import List, Optional, Tuple

from ._decode import Bytesish, to_bytes
from ._errors import (
    ClosedError,
    InvalidInputError,
    ProtocolError,
    TimedOutError,
    UnsupportedError,
    error_from_reply_text,
)
from ._reply import Reply, ReplyKind
from ._resp_conn import RespConn
from ._types import PubsubEvent, PubsubKind
from ._url import TargetKind, parse_connect_url, release_store, resolve_store


class Subscriber:
    def __init__(self, remote: Optional[RespConn], emb: Optional["_EmbSub"]):
        self._remote = remote
        self._emb = emb
        self._read_timeout: Optional[float] = None

    @classmethod
    def connect(cls, url: str) -> "Subscriber":
        """Open a fresh subscriber without subscribing yet (§3.11).
        Anonymous mem:// is rejected."""
        target = parse_connect_url(url)
        if target.kind is TargetKind.MEM_ANON:
            raise UnsupportedError(
                "anonymous mem:// has no other producer; use mem://<name> for a shared bus"
            )
        if target.kind in (TargetKind.MEM_NAMED, TargetKind.FILE):
            db, key = resolve_store(target)
            return cls(None, _EmbSub(db, key))
        conn = RespConn.dial(target.host, target.port)
        return cls(conn, None)

    @classmethod
    def connect_channels(cls, url: str, *channels: Bytesish) -> "Subscriber":
        if not channels:
            raise InvalidInputError(
                "connect_channels needs >= 1 channel — use connect() for an empty start"
            )
        s = cls.connect(url)
        try:
            s.subscribe(*channels)
        except Exception:
            s.close()
            raise
        return s

    def subscribe(self, *channels: Bytesish) -> None:
        if not channels:
            raise InvalidInputError("subscribe needs >= 1 channel")
        chans = [to_bytes(c) for c in channels]
        if self._remote is not None:
            self._remote.write([b"SUBSCRIBE", *chans])
            return
        self._emb.add(chans, pattern=False)

    def psubscribe(self, *patterns: Bytesish) -> None:
        if not patterns:
            raise InvalidInputError("psubscribe needs >= 1 pattern")
        pats = [to_bytes(p) for p in patterns]
        if self._remote is not None:
            self._remote.write([b"PSUBSCRIBE", *pats])
            return
        self._emb.add(pats, pattern=True)

    def unsubscribe(self, *channels: Bytesish) -> None:
        if self._remote is not None:
            self._remote.write([b"UNSUBSCRIBE", *[to_bytes(c) for c in channels]])
            return
        self._emb.remove([to_bytes(c) for c in channels], pattern=False)

    def punsubscribe(self, *patterns: Bytesish) -> None:
        if self._remote is not None:
            self._remote.write([b"PUNSUBSCRIBE", *[to_bytes(p) for p in patterns]])
            return
        self._emb.remove([to_bytes(p) for p in patterns], pattern=True)

    def hello3(self) -> PubsubEvent:
        """Negotiate RESP3 push frames; remote-only, must precede subscribe."""
        if self._remote is None:
            raise UnsupportedError("HELLO 3 is remote/TCP-only; the embedded bus has no proto switch")
        self._remote.write([b"HELLO", b"3"])
        r = self._remote.read_reply()
        if r.kind in (ReplyKind.MAP, ReplyKind.ARRAY):
            return PubsubEvent(kind=PubsubKind.SUBSCRIBE, channel=b"HELLO", count=3)
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"unexpected HELLO 3 reply shape: {r.shape()}")

    def recv(self) -> PubsubEvent:
        """Block for the next pub/sub frame (acks and deliveries)."""
        if self._remote is not None:
            return classify_frame(self._remote.read_reply())
        return classify_frame(self._emb.recv(self._read_timeout))

    def recv_message(self) -> Tuple[bytes, bytes]:
        """Block for the next Message/Pmessage, skipping ack frames."""
        while True:
            ev = self.recv()
            if ev.kind in (PubsubKind.MESSAGE, PubsubKind.PMESSAGE):
                return ev.channel, ev.payload  # type: ignore[return-value]

    def set_read_timeout(self, seconds: Optional[float]) -> None:
        self._read_timeout = seconds
        if self._remote is not None:
            self._remote.set_read_timeout(seconds)

    def events(self):
        """Iterator of PubsubEvent; terminates on ClosedError."""
        while True:
            try:
                yield self.recv()
            except ClosedError:
                return

    def messages(self):
        """Iterator of (channel, payload); ack frames silently skipped."""
        while True:
            try:
                yield self.recv_message()
            except ClosedError:
                return

    def close(self) -> None:
        if self._remote is not None:
            self._remote.close()
            self._remote = None
        if self._emb is not None:
            self._emb.close()
            self._emb = None

    def __enter__(self) -> "Subscriber":
        return self

    def __exit__(self, *exc) -> None:
        self.close()


# --- embedded bus consumer ----------------------------------------------


class _EmbSub:
    """One FFI subscription handle per channel/pattern (the C-ABI pub/sub is
    per-channel). Polls handles round-robin; blocks efficiently on the
    single-handle common case."""

    def __init__(self, db, key: str):
        self._db = db
        self._key = key
        self._handles: List[Tuple[object, bytes, bool]] = []  # (sub, name, pattern)
        self._rr = 0

    def add(self, names: List[bytes], pattern: bool) -> None:
        for n in names:
            sub = self._db.psubscribe(n) if pattern else self._db.subscribe(n)
            self._handles.append((sub, n, pattern))

    def remove(self, names: List[bytes], pattern: bool) -> None:
        keep = []
        for sub, name, pat in self._handles:
            drop = pat == pattern and (not names or name in names)
            if drop:
                sub.close()
            else:
                keep.append((sub, name, pat))
        self._handles = keep

    def recv(self, timeout: Optional[float]) -> Reply:
        end = None if timeout is None else time.monotonic() + timeout
        while True:
            frame = self._poll()
            if frame is not None:
                return frame
            if len(self._handles) == 1:
                wait_ms = 0
                if end is not None:
                    remaining = end - time.monotonic()
                    if remaining <= 0:
                        raise TimedOutError()
                    wait_ms = max(int(remaining * 1000), 1)
                r = self._handles[0][0].wait(wait_ms)
                if r is not None:
                    return r
                if timeout is not None:
                    raise TimedOutError()
                continue
            if end is not None and time.monotonic() >= end:
                raise TimedOutError()
            time.sleep(0.002)

    def _poll(self) -> Optional[Reply]:
        n = len(self._handles)
        for i in range(n):
            sub = self._handles[(self._rr + i) % n][0]
            r = sub.next()
            if r is not None:
                self._rr = (self._rr + i + 1) % n
                return r
        return None

    def close(self) -> None:
        for sub, _, _ in self._handles:
            sub.close()
        self._handles = []
        if self._db is not None:
            release_store(self._key, self._db)
            self._db = None


# --- frame classification -----------------------------------------------


def classify_frame(r: Reply) -> PubsubEvent:
    if r.kind in (ReplyKind.ARRAY, ReplyKind.PUSH):
        items = r.items
    elif r.is_error():
        raise error_from_reply_text(r.data)
    else:
        raise ProtocolError(f"expected array/push frame, got {r.shape()}")
    if not items or items[0].kind is not ReplyKind.BULK:
        raise ProtocolError("pubsub frame missing kind field")
    return _shape_event(items[0].data, items)


def _shape_event(kind: bytes, items: List[Reply]) -> PubsubEvent:
    if kind in (b"subscribe", b"psubscribe", b"unsubscribe", b"punsubscribe"):
        return _ack_event(kind, items)
    if kind == b"message":
        if len(items) != 3 or items[1].kind is not ReplyKind.BULK or items[2].kind is not ReplyKind.BULK:
            raise ProtocolError("bad message frame")
        return PubsubEvent(kind=PubsubKind.MESSAGE, channel=items[1].data, payload=items[2].data)
    if kind == b"pmessage":
        if len(items) != 4 or any(items[i].kind is not ReplyKind.BULK for i in (1, 2, 3)):
            raise ProtocolError("bad pmessage frame")
        return PubsubEvent(
            kind=PubsubKind.PMESSAGE, pattern=items[1].data, channel=items[2].data, payload=items[3].data
        )
    raise ProtocolError(f"unknown pubsub kind {kind!r}")


_ACK_KIND = {
    b"subscribe": PubsubKind.SUBSCRIBE,
    b"psubscribe": PubsubKind.PSUBSCRIBE,
    b"unsubscribe": PubsubKind.UNSUBSCRIBE,
    b"punsubscribe": PubsubKind.PUNSUBSCRIBE,
}
_PATTERN_ACK = {b"psubscribe", b"punsubscribe"}


def _ack_event(kind: bytes, items: List[Reply]) -> PubsubEvent:
    if len(items) != 3:
        raise ProtocolError(f"expected 3-element pubsub frame, got {len(items)}")
    name_item = items[1]
    if name_item.kind is ReplyKind.BULK:
        name: Optional[bytes] = name_item.data
    elif name_item.is_nil():
        name = None
    else:
        raise ProtocolError("bad pubsub name field")
    if items[2].kind is not ReplyKind.INT:
        raise ProtocolError("bad pubsub count field")
    ev = PubsubEvent(kind=_ACK_KIND[kind], count=items[2].integer)
    if kind in _PATTERN_ACK:
        ev.pattern = name
    else:
        ev.channel = name
    return ev

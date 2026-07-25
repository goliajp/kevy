"""Async pub/sub consumer (contract §3.11, async face).

The coroutine twin of :class:`kevy.Subscriber`. Remote uses asyncio streams;
the embedded bus runs its blocking poll/wait in a worker thread. ``events``
and ``messages`` are async iterators (§7).
"""

from __future__ import annotations

import asyncio
from typing import Optional, Tuple

from ._decode import Bytesish, to_bytes
from ._errors import (
    ClosedError,
    InvalidInputError,
    ProtocolError,
    TimedOutError,
    UnsupportedError,
    error_from_reply_text,
)
from ._reply import ReplyKind
from ._resp_conn_async import AsyncRespConn
from ._subscriber import _EmbSub, classify_frame
from ._types import PubsubEvent, PubsubKind
from ._url import TargetKind, parse_connect_url, release_store, resolve_store


class AsyncSubscriber:
    def __init__(self, remote: Optional[AsyncRespConn], emb: Optional[_EmbSub]):
        self._remote = remote
        self._emb = emb
        self._read_timeout: Optional[float] = None

    @classmethod
    async def connect(cls, url: str) -> "AsyncSubscriber":
        target = parse_connect_url(url)
        if target.kind is TargetKind.MEM_ANON:
            raise UnsupportedError(
                "anonymous mem:// has no other producer; use mem://<name> for a shared bus"
            )
        if target.kind in (TargetKind.MEM_NAMED, TargetKind.FILE):
            db, key = await asyncio.to_thread(resolve_store, target)
            return cls(None, _EmbSub(db, key))
        conn = await AsyncRespConn.dial(target.host, target.port)
        return cls(conn, None)

    @classmethod
    async def connect_channels(cls, url: str, *channels: Bytesish) -> "AsyncSubscriber":
        if not channels:
            raise InvalidInputError(
                "connect_channels needs >= 1 channel — use connect() for an empty start"
            )
        s = await cls.connect(url)
        try:
            await s.subscribe(*channels)
        except Exception:
            await s.close()
            raise
        return s

    async def subscribe(self, *channels: Bytesish) -> None:
        if not channels:
            raise InvalidInputError("subscribe needs >= 1 channel")
        chans = [to_bytes(c) for c in channels]
        if self._remote is not None:
            await self._remote.write([b"SUBSCRIBE", *chans])
            return
        await asyncio.to_thread(self._emb.add, chans, False)

    async def psubscribe(self, *patterns: Bytesish) -> None:
        if not patterns:
            raise InvalidInputError("psubscribe needs >= 1 pattern")
        pats = [to_bytes(p) for p in patterns]
        if self._remote is not None:
            await self._remote.write([b"PSUBSCRIBE", *pats])
            return
        await asyncio.to_thread(self._emb.add, pats, True)

    async def unsubscribe(self, *channels: Bytesish) -> None:
        if self._remote is not None:
            await self._remote.write([b"UNSUBSCRIBE", *[to_bytes(c) for c in channels]])
            return
        self._emb.remove([to_bytes(c) for c in channels], pattern=False)

    async def punsubscribe(self, *patterns: Bytesish) -> None:
        if self._remote is not None:
            await self._remote.write([b"PUNSUBSCRIBE", *[to_bytes(p) for p in patterns]])
            return
        self._emb.remove([to_bytes(p) for p in patterns], pattern=True)

    async def hello3(self) -> PubsubEvent:
        if self._remote is None:
            raise UnsupportedError("HELLO 3 is remote/TCP-only; the embedded bus has no proto switch")
        await self._remote.write([b"HELLO", b"3"])
        r = await self._remote.read_reply()
        if r.kind in (ReplyKind.MAP, ReplyKind.ARRAY):
            return PubsubEvent(kind=PubsubKind.SUBSCRIBE, channel=b"HELLO", count=3)
        if r.is_error():
            raise error_from_reply_text(r.data)
        raise ProtocolError(f"unexpected HELLO 3 reply shape: {r.shape()}")

    async def recv(self) -> PubsubEvent:
        if self._remote is not None:
            if self._read_timeout is not None:
                try:
                    r = await asyncio.wait_for(self._remote.read_reply(), self._read_timeout)
                except asyncio.TimeoutError:
                    raise TimedOutError()
            else:
                r = await self._remote.read_reply()
            return classify_frame(r)
        r = await asyncio.to_thread(self._emb.recv, self._read_timeout)
        return classify_frame(r)

    async def recv_message(self) -> Tuple[bytes, bytes]:
        while True:
            ev = await self.recv()
            if ev.kind in (PubsubKind.MESSAGE, PubsubKind.PMESSAGE):
                return ev.channel, ev.payload  # type: ignore[return-value]

    def set_read_timeout(self, seconds: Optional[float]) -> None:
        self._read_timeout = seconds

    async def events(self):
        while True:
            try:
                yield await self.recv()
            except ClosedError:
                return

    async def messages(self):
        while True:
            try:
                yield await self.recv_message()
            except ClosedError:
                return

    async def close(self) -> None:
        if self._remote is not None:
            self._remote.close()
            self._remote = None
        if self._emb is not None:
            emb = self._emb
            self._emb = None
            await asyncio.to_thread(emb.close)

    async def __aenter__(self) -> "AsyncSubscriber":
        return self

    async def __aexit__(self, *exc) -> None:
        await self.close()

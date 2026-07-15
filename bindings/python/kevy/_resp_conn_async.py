"""Asyncio RESP2/RESP3 TCP transport (contract §1.1, §1.4 async face).

The coroutine twin of :class:`kevy._resp_conn.RespConn`, over
``asyncio.StreamReader``/``StreamWriter``. One in-flight request at a time;
the read buffer reassembles multi-segment replies. A clean server close
mid-read surfaces as ``ClosedError``; other socket failures as ``IoError``.
"""

from __future__ import annotations

import asyncio
import socket
from typing import List, Optional

from ._errors import ClosedError, IoError, ProtocolError
from ._reply import Reply, parse_reply
from ._resp_conn import encode_command


class AsyncRespConn:
    def __init__(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter, host: str, port: int):
        self._reader: Optional[asyncio.StreamReader] = reader
        self._writer: Optional[asyncio.StreamWriter] = writer
        self._rbuf = bytearray()
        self.host = host
        self.port = port

    @classmethod
    async def dial(cls, host: str, port: int) -> "AsyncRespConn":
        try:
            reader, writer = await asyncio.open_connection(host, port)
        except OSError as e:
            raise IoError(e)
        sock = writer.get_extra_info("socket")
        if sock is not None:
            try:
                sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            except OSError:
                pass
        return cls(reader, writer, host, port)

    def close(self) -> None:
        if self._writer is not None:
            try:
                self._writer.close()
            except OSError:
                pass
            self._writer = None
            self._reader = None

    @property
    def closed(self) -> bool:
        return self._writer is None

    async def request(self, argv: List[bytes]) -> Reply:
        if self._writer is None:
            raise ClosedError()
        await self._write_all(encode_command(argv))
        return await self._read_one()

    async def pipeline_raw(self, wire: bytes, n: int) -> List[Reply]:
        if self._writer is None:
            raise ClosedError()
        await self._write_all(wire)
        out: List[Reply] = []
        while len(out) < n:
            out.append(await self._read_one())
        return out

    async def write(self, argv: List[bytes]) -> None:
        if self._writer is None:
            raise ClosedError()
        await self._write_all(encode_command(argv))

    async def read_reply(self) -> Reply:
        if self._reader is None:
            raise ClosedError()
        return await self._read_one()

    async def select_db(self, db: int) -> None:
        r = await self.request([b"SELECT", str(int(db)).encode()])
        if r.is_error():
            raise ProtocolError(f"SELECT {db} rejected: {r.text()}")

    # --- internals ------------------------------------------------------

    async def _write_all(self, wire: bytes) -> None:
        assert self._writer is not None
        try:
            self._writer.write(wire)
            await self._writer.drain()
        except OSError as e:
            self.close()
            raise IoError(e)

    async def _read_one(self) -> Reply:
        assert self._reader is not None
        while True:
            try:
                reply, used = parse_reply(bytes(self._rbuf))
            except ProtocolError:
                raise ProtocolError("malformed reply")
            if used > 0:
                del self._rbuf[:used]
                return reply  # type: ignore[return-value]
            try:
                chunk = await self._reader.read(8192)
            except OSError as e:
                self.close()
                raise IoError(e)
            if not chunk:
                self.close()
                raise ClosedError()
            self._rbuf.extend(chunk)

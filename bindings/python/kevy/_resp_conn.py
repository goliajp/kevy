"""Blocking RESP2/RESP3 TCP transport (contract §1.1).

One request at a time; a read buffer reassembles multi-segment replies. A
clean server close mid-read surfaces as ``ClosedError``; a socket read
timeout as ``TimedOutError``; any other socket failure as ``IoError``. Not
safe for concurrent use from multiple threads.
"""

from __future__ import annotations

import socket
from typing import List, Optional

from ._errors import ClosedError, IoError, ProtocolError, TimedOutError
from ._reply import Reply, parse_reply


def encode_command(argv: List[bytes]) -> bytes:
    parts = [b"*", str(len(argv)).encode(), b"\r\n"]
    for a in argv:
        parts.extend([b"$", str(len(a)).encode(), b"\r\n", a, b"\r\n"])
    return b"".join(parts)


class RespConn:
    def __init__(self, sock: socket.socket, host: str, port: int):
        self._sock: Optional[socket.socket] = sock
        self._rbuf = bytearray()
        self.host = host
        self.port = port

    @classmethod
    def dial(cls, host: str, port: int) -> "RespConn":
        try:
            sock = socket.create_connection((host, port))
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        except OSError as e:
            raise IoError(e)
        return cls(sock, host, port)

    def close(self) -> None:
        if self._sock is not None:
            try:
                self._sock.close()
            except OSError:
                pass
            self._sock = None

    @property
    def closed(self) -> bool:
        return self._sock is None

    def request(self, argv: List[bytes]) -> Reply:
        if self._sock is None:
            raise ClosedError()
        self._write_all(encode_command(argv))
        return self._read_one()

    def pipeline_raw(self, wire: bytes, n: int) -> List[Reply]:
        if self._sock is None:
            raise ClosedError()
        self._write_all(wire)
        out: List[Reply] = []
        while len(out) < n:
            out.append(self._read_one())
        return out

    def write(self, argv: List[bytes]) -> None:
        """Send one command without reading a reply — the decoupled send
        side used by the pub/sub consumer."""
        if self._sock is None:
            raise ClosedError()
        self._write_all(encode_command(argv))

    def read_reply(self) -> Reply:
        """Block for the next reply frame (used by the pub/sub consumer)."""
        if self._sock is None:
            raise ClosedError()
        return self._read_one()

    def set_read_timeout(self, seconds: Optional[float]) -> None:
        if self._sock is None:
            raise ClosedError()
        self._sock.settimeout(seconds)

    def select_db(self, db: int) -> None:
        r = self.request([b"SELECT", str(int(db)).encode()])
        if r.is_error():
            raise ProtocolError(f"SELECT {db} rejected: {r.text()}")

    # --- internals ------------------------------------------------------

    def _write_all(self, wire: bytes) -> None:
        assert self._sock is not None
        try:
            self._sock.sendall(wire)
        except socket.timeout:
            raise TimedOutError()
        except OSError as e:
            self.close()
            raise IoError(e)

    def _read_one(self) -> Reply:
        assert self._sock is not None
        while True:
            try:
                reply, used = parse_reply(bytes(self._rbuf))
            except ProtocolError:
                raise ProtocolError("malformed reply")
            if used > 0:
                del self._rbuf[:used]
                return reply  # type: ignore[return-value]
            try:
                chunk = self._sock.recv(8192)
            except socket.timeout:
                raise TimedOutError()
            except OSError as e:
                self.close()
                raise IoError(e)
            if not chunk:
                self.close()
                raise ClosedError()
            self._rbuf.extend(chunk)

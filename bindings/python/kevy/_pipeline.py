"""Non-atomic pipeline batching buffer (contract §3.13).

Queue N commands client-side; the client sends them as one write and reads
N replies in order. NOT atomic. Per-command errors come back as
``Reply``-of-ERROR entries inline (they do not abort the batch). An empty
argv poisons the batch, surfaced as ``InvalidInputError`` at send time.
"""

from __future__ import annotations

from typing import List

from ._decode import Bytesish, to_bytes
from ._resp_conn import encode_command


class PipelineBuf:
    def __init__(self) -> None:
        self._chunks: List[bytes] = []
        self.length = 0
        self.poisoned = False

    def cmd(self, *parts: Bytesish) -> "PipelineBuf":
        """Queue one command (verb + args). Chainable; empty parts poisons
        the batch."""
        if not parts:
            self.poisoned = True
            return self
        self._chunks.append(encode_command([to_bytes(p) for p in parts]))
        self.length += 1
        return self

    def __len__(self) -> int:
        return self.length

    def is_empty(self) -> bool:
        return self.length == 0

    @property
    def wire(self) -> bytes:
        return b"".join(self._chunks)

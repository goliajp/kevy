"""Public data types (contract §4): the field-by-field result shapes the
typed command families return."""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import List, Optional


@dataclass
class ZMember:
    """A (score, member) pair for ZADD (§3.5)."""

    score: float
    member: bytes


@dataclass
class KV:
    """A (key, value) blocking-pop hit (§3.14)."""

    key: bytes
    value: bytes


@dataclass
class ZPopHit:
    """A (key, member, score) BZPOPMIN hit (§4.9)."""

    key: bytes
    member: bytes
    score: float


@dataclass
class IdxRow:
    """One IDX.QUERY hit: row key + the indexed value's wire form (§4.3)."""

    key: bytes
    value: bytes


@dataclass
class IdxPage:
    """One page of IDX.QUERY RANGE/EQ results (§4.4).

    ``cursor`` is None when the scan is complete (wire cursor "0"); else pass
    it back to resume. ``rows`` are hits in (value, key) order.
    """

    cursor: Optional[bytes]
    rows: List[IdxRow] = field(default_factory=list)


@dataclass
class IdxInfo:
    """One IDX.LIST entry (§4.5). Unknown labels are skipped."""

    name: bytes = b""
    prefix: bytes = b""
    kind: str = ""  # "range" | "unique" | "text" | "ann" | "agg"
    state: str = ""  # "ready" | "building"
    entries: int = 0
    bytes_: int = 0


@dataclass
class Ranked:
    """A (key, score/distance) result from IDX.QUERY MATCH / KNN (§3.8)."""

    key: bytes
    score: float


@dataclass
class FeedFrame:
    """One applied effect's argv at a stream offset (§4.6)."""

    offset: int
    argv: List[bytes] = field(default_factory=list)


@dataclass
class FeedBatch:
    """A run of frames plus the cursor to resume from (§4.7)."""

    generation: int
    next_offset: int
    frames: List[FeedFrame] = field(default_factory=list)


class PubsubKind(Enum):
    SUBSCRIBE = "subscribe"
    PSUBSCRIBE = "psubscribe"
    UNSUBSCRIBE = "unsubscribe"
    PUNSUBSCRIBE = "punsubscribe"
    MESSAGE = "message"
    PMESSAGE = "pmessage"


@dataclass
class PubsubEvent:
    """One received pub/sub frame — acks and deliveries alike (§4.8).

    ``count`` = total channels + patterns held after the op. For
    unsubscribe/punsubscribe a None channel/pattern means "all".
    """

    kind: PubsubKind
    channel: Optional[bytes] = None
    pattern: Optional[bytes] = None
    payload: Optional[bytes] = None
    count: int = 0

"""Enums shared with the embedded store and the wire protocol.

See the kevy client contract §4.13 / §4.2. These map the Rust reference
enums onto native Python ``enum.Enum`` types.
"""

from __future__ import annotations

from enum import Enum


class ZAggregate(Enum):
    """AGGREGATE mode for ZINTERSTORE / ZUNIONSTORE (§3.6).

    The default, ``SUM``, emits no wire tag.
    """

    SUM = "SUM"
    MIN = "MIN"
    MAX = "MAX"

    def tag(self) -> bytes:
        return self.value.encode()


class HExpireCond(Enum):
    """Optional condition on the hash field-TTL verbs (§3.7).

    ``ALWAYS`` emits no wire keyword; the rest emit their name.
    """

    ALWAYS = None
    NX = "NX"
    XX = "XX"
    GT = "GT"
    LT = "LT"

    def keyword(self) -> bytes | None:
        return None if self.value is None else self.value.encode()


class IdxType(Enum):
    """Declared scalar type of a range index (§4.2)."""

    I64 = "i64"
    F64 = "f64"
    STR = "str"

    def tag(self) -> bytes:
        return self.value.encode()


class RespVersion(Enum):
    """Per-connection RESP dialect (§4.1). RESP2 is the default."""

    V2 = 2
    V3 = 3


class EvictionPolicy(Enum):
    """Embedded store maxmemory behaviour (§5.3)."""

    NO_EVICTION = "noeviction"
    ALLKEYS_LRU = "allkeys-lru"
    ALLKEYS_LFU = "allkeys-lfu"
    ALLKEYS_RANDOM = "allkeys-random"
    VOLATILE_LRU = "volatile-lru"
    VOLATILE_LFU = "volatile-lfu"
    VOLATILE_RANDOM = "volatile-random"
    VOLATILE_TTL = "volatile-ttl"

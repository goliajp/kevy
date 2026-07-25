"""kevy — the first-party Python client for the pure-Rust, Redis-compatible
kevy engine.

One package, both faces (contract §1.4): a blocking :class:`Client` and an
asyncio :class:`AsyncClient`, exactly as redis-py ships ``redis`` +
``redis.asyncio``. A single ``connect(url)`` chooses the backend from the URL
scheme (§1.1): ``mem://`` / ``file://`` open the in-process **embedded**
engine (a ctypes binding over the C ABI ``libkevy_ffi`` — the new Python
embedded door, §5); ``kevy://`` / ``redis://`` / ``tcp://`` open a native
RESP2/3 TCP client. The wire is bytes; every key/value is binary-safe (§7).

    import kevy

    c = kevy.connect("mem://app")          # embedded, blocking
    c.set("k", "v"); print(c.get(b"k"))    # -> b"v"

    ac = await kevy.AsyncClient.connect("kevy://127.0.0.1:6379")
    await ac.set("k", "v")

Errors are raised as a :class:`KevyError` hierarchy inspectable by type (§2).
"""

from __future__ import annotations

from ._client import Client
from ._client_async import AsyncClient
from ._cluster import ClusterClient, crc16, key_hash_slot
from ._embedded import DB, Sub, abi, open_mem, open_persistent, version
from ._enums import EvictionPolicy, HExpireCond, IdxType, RespVersion, ZAggregate
from ._errors import (
    ClosedError,
    InvalidInputError,
    IoError,
    KevyError,
    NoSuchKeyError,
    NotFloatError,
    NotFoundError,
    NotIntegerError,
    OutOfMemoryStoreError,
    OutOfRangeError,
    OverflowStoreError,
    ProtocolError,
    ReadOnlyError,
    StoreError,
    StoreErrorKind,
    TimedOutError,
    UnsupportedError,
    WrongTypeError,
)
from ._pipeline import PipelineBuf
from ._reply import Reply, ReplyKind
from ._subscriber import Subscriber
from ._subscriber_async import AsyncSubscriber
from ._transaction import Transaction, TransactionReplies
from ._transaction_async import AsyncTransaction
from ._types import (
    FeedBatch,
    FeedFrame,
    IdxInfo,
    IdxPage,
    IdxRow,
    PubsubEvent,
    PubsubKind,
    Ranked,
    ZMember,
    ZPopHit,
    KV,
)

__version__ = "4.0.0"


def connect(url: str) -> Client:
    """Open a blocking :class:`Client` for ``url`` (§1.1). Equivalent to
    :meth:`Client.connect`."""
    return Client.connect(url)


async def aconnect(url: str) -> AsyncClient:
    """Open an asyncio :class:`AsyncClient` for ``url`` (§1.4). Equivalent to
    :meth:`AsyncClient.connect`."""
    return await AsyncClient.connect(url)


__all__ = [
    "__version__",
    "connect",
    "aconnect",
    # clients
    "Client",
    "AsyncClient",
    "ClusterClient",
    "Subscriber",
    "AsyncSubscriber",
    "Transaction",
    "AsyncTransaction",
    "TransactionReplies",
    "PipelineBuf",
    # embedded store
    "DB",
    "Sub",
    "open_mem",
    "open_persistent",
    "abi",
    "version",
    # reply / enums
    "Reply",
    "ReplyKind",
    "ZAggregate",
    "HExpireCond",
    "IdxType",
    "RespVersion",
    "EvictionPolicy",
    # data types
    "ZMember",
    "KV",
    "ZPopHit",
    "IdxRow",
    "IdxPage",
    "IdxInfo",
    "Ranked",
    "FeedFrame",
    "FeedBatch",
    "PubsubEvent",
    "PubsubKind",
    # cluster helpers
    "crc16",
    "key_hash_slot",
    # errors
    "KevyError",
    "StoreError",
    "StoreErrorKind",
    "WrongTypeError",
    "NotIntegerError",
    "OverflowStoreError",
    "OutOfRangeError",
    "NoSuchKeyError",
    "NotFloatError",
    "OutOfMemoryStoreError",
    "IoError",
    "ProtocolError",
    "ReadOnlyError",
    "InvalidInputError",
    "NotFoundError",
    "UnsupportedError",
    "TimedOutError",
    "ClosedError",
]

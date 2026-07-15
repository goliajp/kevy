"""The KevyError exception hierarchy (contract §2).

The Rust reference is errors-as-values; the Python port raises a
``KevyError`` hierarchy (contract §2.1, Python row). The *variant identity*
survives as the exception subclass, so callers branch by ``isinstance`` and
the conformance tests assert on variant on both backends.
"""

from __future__ import annotations

from enum import Enum
from typing import Optional


class StoreErrorKind(Enum):
    """Structured store-semantic error carried by a StoreError (§2.3)."""

    WRONG_TYPE = "WrongType"
    NOT_INTEGER = "NotInteger"
    OVERFLOW = "Overflow"
    OUT_OF_RANGE = "OutOfRange"
    NO_SUCH_KEY = "NoSuchKey"
    NOT_FLOAT = "NotFloat"
    OUT_OF_MEMORY = "OutOfMemory"


class KevyError(Exception):
    """Base of every error raised by the kevy client (contract §2.2)."""


class StoreError(KevyError):
    """A structured store-semantic error (Redis WRONGTYPE, OOM, ...).

    ``kind`` distinguishes the variant; concrete subclasses exist for each so
    callers may match either by subclass or by ``kind``.
    """

    kind: StoreErrorKind = StoreErrorKind.WRONG_TYPE  # overridden per subclass

    def __init__(self, message: Optional[str] = None):
        super().__init__(message or f"store error: {self.kind.value}")


class WrongTypeError(StoreError):
    kind = StoreErrorKind.WRONG_TYPE


class NotIntegerError(StoreError):
    kind = StoreErrorKind.NOT_INTEGER


class OverflowStoreError(StoreError):
    kind = StoreErrorKind.OVERFLOW


class OutOfRangeError(StoreError):
    kind = StoreErrorKind.OUT_OF_RANGE


class NoSuchKeyError(StoreError):
    kind = StoreErrorKind.NO_SUCH_KEY


class NotFloatError(StoreError):
    kind = StoreErrorKind.NOT_FLOAT


class OutOfMemoryStoreError(StoreError):
    kind = StoreErrorKind.OUT_OF_MEMORY


_STORE_BY_KIND = {
    StoreErrorKind.WRONG_TYPE: WrongTypeError,
    StoreErrorKind.NOT_INTEGER: NotIntegerError,
    StoreErrorKind.OVERFLOW: OverflowStoreError,
    StoreErrorKind.OUT_OF_RANGE: OutOfRangeError,
    StoreErrorKind.NO_SUCH_KEY: NoSuchKeyError,
    StoreErrorKind.NOT_FLOAT: NotFloatError,
    StoreErrorKind.OUT_OF_MEMORY: OutOfMemoryStoreError,
}


class IoError(KevyError):
    """OS/transport failure (socket, file, AOF). Wraps the cause."""

    def __init__(self, cause: object):
        self.cause = cause
        super().__init__(f"io error: {cause}")


class ProtocolError(KevyError):
    """A server ``-ERR`` reply (text verbatim) or a malformed/unexpected
    reply shape. A successful round-trip carrying an error frame."""


class ReadOnlyError(KevyError):
    """Write rejected: the target is a read-only replica."""

    def __init__(self, message: str = "READONLY You can't write against a read only replica"):
        super().__init__(message)


class InvalidInputError(KevyError):
    """Bad argument to a typed API — rejected before touching state."""


class NotFoundError(KevyError):
    """A named object (index, view, key) does not exist."""


class UnsupportedError(KevyError):
    """Op unavailable on this backend/build (e.g. IDX.* on embedded, TLS)."""


class TimedOutError(KevyError):
    """A bounded blocking call ran out its timeout."""

    def __init__(self, message: str = "timed out"):
        super().__init__(message)


class ClosedError(KevyError):
    """The connection / in-process bus is gone (EOF)."""

    def __init__(self, message: str = "connection closed"):
        super().__init__(message)


def store_error_for(text: bytes) -> Optional[StoreError]:
    """Map a server error frame's text onto the structured taxonomy, or
    None when the text is not a recognised store-semantic error (§2.2)."""
    s = text.decode("utf-8", "replace")
    if s.startswith("WRONGTYPE"):
        return WrongTypeError(s)
    if s.startswith("OOM"):
        return OutOfMemoryStoreError(s)
    if "is not an integer" in s:
        return NotIntegerError(s)
    if "is not a valid float" in s:
        return NotFloatError(s)
    if "would overflow" in s:
        return OverflowStoreError(s)
    if "no such key" in s:
        return NoSuchKeyError(s)
    if "index out of range" in s:
        return OutOfRangeError(s)
    return None


def error_from_reply_text(text: bytes) -> KevyError:
    """A Reply::Error's text → StoreError when recognised, else a
    ProtocolError carrying the wire text verbatim (§2.2)."""
    store = store_error_for(text)
    if store is not None:
        return store
    return ProtocolError(text.decode("utf-8", "replace"))

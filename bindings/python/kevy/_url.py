"""Connect-URL parsing and the process-global embedded registry (§1.1–§1.3).

A single ``connect(url)`` chooses the backend from the scheme. Embedded
targets (``mem://<name>`` / ``file://<path>``) share one backing Store per
URL through a process-global, refcounted registry so a publisher and a
consumer opened on the same URL find each other (§1.3). Anonymous ``mem://``
is always isolated.
"""

from __future__ import annotations

import os
import threading
from dataclasses import dataclass
from enum import Enum, auto
from typing import Optional, Tuple

from ._errors import InvalidInputError, UnsupportedError


class TargetKind(Enum):
    MEM_ANON = auto()  # mem:// — fresh, never shared
    MEM_NAMED = auto()  # mem://<name> — shared by name
    FILE = auto()  # file://<path> — shared + persistent
    REMOTE = auto()  # kevy:// / redis:// / tcp://


@dataclass
class Target:
    kind: TargetKind
    url: str
    name: str = ""  # mem://<name>
    path: str = ""  # file://<canonical-path>
    host: str = ""  # remote host
    port: int = 6379  # remote port (default 6379)
    db: Optional[int] = None  # remote /db (None for tcp://)

    def registry_key(self) -> str:
        if self.kind is TargetKind.MEM_NAMED:
            return "mem://" + self.name
        if self.kind is TargetKind.FILE:
            return "file://" + self.path
        return ""


def parse_connect_url(url: str) -> Target:
    """Resolve a connect URL to a Target, rejecting TLS/AUTH and unknown
    schemes before any I/O (§1.1)."""
    if "://" not in url:
        raise InvalidInputError("URL missing '://'")
    scheme, rest = url.split("://", 1)
    if scheme == "mem":
        if rest == "":
            return Target(kind=TargetKind.MEM_ANON, url=url)
        return Target(kind=TargetKind.MEM_NAMED, url=url, name=rest)
    if scheme == "file":
        if rest == "":
            raise InvalidInputError(
                "file:// URL must include a path (e.g. file:///var/lib/myapp)"
            )
        return Target(kind=TargetKind.FILE, url=url, path=_canonical_file_path(rest))
    if scheme in ("kevy", "redis", "tcp"):
        return _parse_remote(scheme, rest, url)
    if scheme in ("rediss", "kevys"):
        raise UnsupportedError(
            "TLS schemes (rediss://, kevys://) are unsupported — kevy has no TLS"
        )
    raise InvalidInputError(f"unknown URL scheme '{scheme}://'")


def _canonical_file_path(rest: str) -> str:
    # file:///abs → /abs ; file://./rel → ./rel. Canonicalise so two URLs
    # naming the same path share a store.
    path = rest
    return os.path.abspath(path)


def _parse_remote(scheme: str, rest: str, url: str) -> Target:
    if "@" in rest:
        raise UnsupportedError(
            "userinfo (user:pass@host) is unsupported — kevy has no AUTH"
        )
    authority = rest
    path = ""
    slash = rest.find("/")
    if slash >= 0:
        authority = rest[:slash]
        path = rest[slash + 1 :]
    host, port = _parse_authority(authority)
    target = Target(kind=TargetKind.REMOTE, url=url, host=host, port=port)
    # tcp:// is raw: it never carries a db (§1.1). kevy:// / redis:// honour /N.
    if scheme != "tcp" and path != "":
        try:
            n = int(path)
        except ValueError:
            raise InvalidInputError(
                f"bad db index: '{path}' (expected a non-negative integer)"
            )
        if n < 0:
            raise InvalidInputError(f"bad db index: '{path}' (must be non-negative)")
        target.db = n
    return target


def _parse_authority(authority: str) -> Tuple[str, int]:
    host = authority
    port = 6379
    colon = authority.rfind(":")
    if colon >= 0:
        host = authority[:colon]
        try:
            port = int(authority[colon + 1 :])
        except ValueError:
            raise InvalidInputError(f"bad port: {authority[colon + 1:]}")
        if not (0 <= port <= 65535):
            raise InvalidInputError(f"bad port: {authority[colon + 1:]}")
    if host == "":
        raise InvalidInputError("empty host")
    return host, port


# --- process-global embedded registry (§1.3) ----------------------------


class _SharedStore:
    __slots__ = ("db", "refs")

    def __init__(self, db):
        self.db = db
        self.refs = 1


_registry_lock = threading.Lock()
_registry: dict = {}


def resolve_store(target: Target):
    """Open (or share) the embedded store for a target. Returns
    ``(db, key)``; ``key`` is "" for the unshared anonymous case (§1.3)."""
    from ._embedded import open_mem, open_persistent

    key = target.registry_key()
    if key:
        with _registry_lock:
            entry = _registry.get(key)
            if entry is not None:
                entry.refs += 1
                return entry.db, key

    if target.kind in (TargetKind.MEM_ANON, TargetKind.MEM_NAMED):
        db = open_mem()
    elif target.kind is TargetKind.FILE:
        db = open_persistent(target.path)
    else:
        raise InvalidInputError("resolve_store called on a non-embedded target")

    if not key:
        return db, ""

    with _registry_lock:
        entry = _registry.get(key)
        if entry is not None:  # lost a race — use theirs, close ours
            entry.refs += 1
            db.close()
            return entry.db, key
        _registry[key] = _SharedStore(db)
        return db, key


def release_store(key: str, db) -> None:
    """Drop one reference; close the store when the last handle goes."""
    if not key:
        db.close()
        return
    with _registry_lock:
        entry = _registry.get(key)
        if entry is None:
            return
        entry.refs -= 1
        if entry.refs <= 0:
            entry.db.close()
            del _registry[key]

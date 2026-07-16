"""kevy-embedded: the in-process Store over the C ABI, via ctypes.

This is the NEW Python embedded door the contract's §5 asks for. It binds
``libkevy_ffi`` (crates/kevy-ffi, ``KEVY_ABI = 1``) with ctypes — no
C-extension build — pairing the one generic ``kevy_cmd`` argv path with the
RESP parser in :mod:`kevy._reply`. All ~184 verbs are reachable through
``DB.cmd``; ``DB.get``/``DB.set`` are the scalar fast paths; ``DB.subscribe``
gives a polled/blocking subscription handle (§5.2). The wire is bytes.
"""

from __future__ import annotations

import ctypes
import os
import sys
import threading
from typing import List, Optional

from ._errors import IoError
from ._reply import Reply, decode_reply


class KevyBuf(ctypes.Structure):
    """A kevy-owned buffer (§5.1). Free via ``kevy_buf_free`` — never libc
    free. ``ptr`` is null when ``len == 0``."""

    _fields_ = [
        ("ptr", ctypes.POINTER(ctypes.c_uint8)),
        ("len", ctypes.c_size_t),
        ("cap", ctypes.c_size_t),
    ]


_U8P = ctypes.POINTER(ctypes.c_uint8)
_EMPTY = ctypes.create_string_buffer(1)  # stable non-null ptr for 0-length args


def _dylib_name() -> str:
    if sys.platform == "darwin":
        return "libkevy_ffi.dylib"
    if sys.platform in ("win32", "cygwin"):
        return "kevy_ffi.dll"
    return "libkevy_ffi.so"


def _candidate_paths() -> List[str]:
    name = _dylib_name()
    env = os.environ.get("KEVY_FFI_LIB")
    paths: List[str] = []
    if env:
        paths.append(env)
    here = os.path.dirname(os.path.abspath(__file__))
    # bindings/python/kevy → repo root is three levels up.
    repo = os.path.abspath(os.path.join(here, "..", "..", ".."))
    for profile in ("release", "debug"):
        paths.append(os.path.join(repo, "target", profile, name))
    return paths


def _load_library() -> ctypes.CDLL:
    tried = []
    for path in _candidate_paths():
        if path and os.path.exists(path):
            return ctypes.CDLL(path)
        tried.append(path)
    raise IoError(
        "libkevy_ffi not found; set KEVY_FFI_LIB or build it "
        "(cargo build --release -p kevy-ffi). Tried: " + ", ".join(tried)
    )


_lib_lock = threading.Lock()
_lib: Optional[ctypes.CDLL] = None


def _library() -> ctypes.CDLL:
    global _lib
    if _lib is not None:
        return _lib
    with _lib_lock:
        if _lib is None:
            lib = _load_library()
            _bind_signatures(lib)
            _lib = lib
    return _lib


def _bind_signatures(lib: ctypes.CDLL) -> None:
    lib.kevy_abi.restype = ctypes.c_uint32
    lib.kevy_abi.argtypes = []
    lib.kevy_version.restype = ctypes.c_char_p
    lib.kevy_version.argtypes = []
    lib.kevy_open.restype = ctypes.c_void_p
    lib.kevy_open.argtypes = [_U8P, ctypes.c_size_t]
    lib.kevy_open_mem.restype = ctypes.c_void_p
    lib.kevy_open_mem.argtypes = []
    lib.kevy_close.restype = None
    lib.kevy_close.argtypes = [ctypes.c_void_p]
    lib.kevy_cmd.restype = ctypes.c_int32
    lib.kevy_cmd.argtypes = [
        ctypes.c_void_p,
        ctypes.c_size_t,
        ctypes.POINTER(_U8P),
        ctypes.POINTER(ctypes.c_size_t),
        ctypes.POINTER(KevyBuf),
    ]
    lib.kevy_buf_free.restype = None
    lib.kevy_buf_free.argtypes = [_U8P, ctypes.c_size_t, ctypes.c_size_t]
    lib.kevy_get.restype = ctypes.c_int32
    lib.kevy_get.argtypes = [ctypes.c_void_p, _U8P, ctypes.c_size_t, ctypes.POINTER(KevyBuf)]
    # Zero-copy GET lane: a bulk value returns an Arc clone (no engine copy);
    # freed with kevy_buf_free_shared (paired). ctypes.string_at makes the one
    # copy into Python bytes.
    lib.kevy_get_shared.restype = ctypes.c_int32
    lib.kevy_get_shared.argtypes = [ctypes.c_void_p, _U8P, ctypes.c_size_t, ctypes.POINTER(KevyBuf)]
    lib.kevy_buf_free_shared.restype = None
    lib.kevy_buf_free_shared.argtypes = [_U8P, ctypes.c_size_t, ctypes.c_size_t]
    lib.kevy_set.restype = ctypes.c_int32
    lib.kevy_set.argtypes = [
        ctypes.c_void_p,
        _U8P,
        ctypes.c_size_t,
        _U8P,
        ctypes.c_size_t,
        ctypes.c_uint64,
    ]
    lib.kevy_subscribe.restype = ctypes.c_void_p
    lib.kevy_subscribe.argtypes = [ctypes.c_void_p, _U8P, ctypes.c_size_t]
    lib.kevy_psubscribe.restype = ctypes.c_void_p
    lib.kevy_psubscribe.argtypes = [ctypes.c_void_p, _U8P, ctypes.c_size_t]
    lib.kevy_sub_next.restype = ctypes.c_int32
    lib.kevy_sub_next.argtypes = [ctypes.c_void_p, ctypes.POINTER(KevyBuf)]
    lib.kevy_sub_wait.restype = ctypes.c_int32
    lib.kevy_sub_wait.argtypes = [ctypes.c_void_p, ctypes.c_uint64, ctypes.POINTER(KevyBuf)]
    lib.kevy_sub_close.restype = None
    lib.kevy_sub_close.argtypes = [ctypes.c_void_p]


def _ptr_of(buf) -> "ctypes._Pointer":
    return ctypes.cast(buf, _U8P)


def _bytes_arg(data: bytes):
    """A (ptr, len) pair for one argument, plus a keep-alive owner. A
    non-null pointer is used even for 0-length (§5.1).

    The C ABI takes ``*const u8`` with an explicit length, so the immutable
    ``bytes`` buffer is wrapped in place (no copy) — the returned owner keeps
    it alive across the call. Binary-safe: the length is passed separately, so
    an embedded NUL in ``data`` is never treated as a terminator."""
    n = len(data)
    if n == 0:
        return _ptr_of(_EMPTY), 0, None
    owner = ctypes.c_char_p(data)  # borrows the bytes buffer, no copy
    return ctypes.cast(owner, _U8P), n, owner


def _take_buf(lib: ctypes.CDLL, out: KevyBuf) -> bytes:
    """Copy a returned KevyBuf into Python bytes and free it (§5.1)."""
    if out.len == 0:
        lib.kevy_buf_free(out.ptr, out.len, out.cap)
        return b""
    data = ctypes.string_at(out.ptr, out.len)
    lib.kevy_buf_free(out.ptr, out.len, out.cap)
    return data


def _take_buf_shared(lib: ctypes.CDLL, out: KevyBuf) -> bytes:
    """Like :func:`_take_buf`, but for a buffer from kevy_get_shared — freed
    via the paired kevy_buf_free_shared (drops the engine Arc)."""
    data = b"" if out.len == 0 else ctypes.string_at(out.ptr, out.len)
    lib.kevy_buf_free_shared(out.ptr, out.len, out.cap)
    return data


def abi() -> int:
    """Runtime C ABI version."""
    return int(_library().kevy_abi())


def version() -> str:
    """Engine version, e.g. "4.0.0"."""
    raw = _library().kevy_version()
    return raw.decode() if raw else ""


class DB:
    """An open embedded engine — the kevy-embedded surface (§5.2). Close it
    exactly once. Every method is safe from multiple threads (the C ABI
    serialises internally); ctypes releases the GIL across the call."""

    __slots__ = ("_p", "_lib")

    def __init__(self, handle: int):
        self._p = handle
        self._lib = _library()

    def close(self) -> None:
        if self._p:
            self._lib.kevy_close(self._p)
            self._p = 0

    def __del__(self):  # best-effort; explicit close() is the contract
        try:
            self.close()
        except Exception:
            pass

    def cmd(self, *argv: bytes) -> Reply:
        """Run one command (argv[0] is the verb) — the universal path
        through which every kevy verb is reachable (§5.2). A protocol -ERR
        arrives as a Reply of ERROR kind, not an exception."""
        if not self._p:
            raise IoError("kevy: closed handle")
        if not argv:
            raise IoError("kevy: empty argv")
        argc = len(argv)
        ptrs = (_U8P * argc)()
        lens = (ctypes.c_size_t * argc)()
        owners = []  # keep string buffers alive across the call
        for i, a in enumerate(argv):
            ptr, n, owner = _bytes_arg(a)
            ptrs[i] = ptr
            lens[i] = n
            if owner is not None:
                owners.append(owner)
        out = KevyBuf()
        rc = self._lib.kevy_cmd(self._p, argc, ptrs, lens, ctypes.byref(out))
        # owners kept alive until here
        del owners
        if rc != 0:
            raise IoError("kevy: kevy_cmd misuse")
        data = _take_buf(self._lib, out)
        if not data:
            raise IoError("kevy: empty reply")
        return decode_reply(data)

    def get(self, key: bytes) -> Optional[bytes]:
        """Scalar fast GET (no argv/RESP framing, §5.2). None on miss."""
        if not self._p:
            raise IoError("kevy: closed handle")
        kp, kn, owner = _bytes_arg(key)
        out = KevyBuf()
        rc = self._lib.kevy_get_shared(self._p, kp, kn, ctypes.byref(out))
        del owner
        if rc < 0:
            raise IoError("kevy: kevy_get_shared misuse")
        if rc == 0:
            return None
        return _take_buf_shared(self._lib, out)

    def set(self, key: bytes, value: bytes, ttl_ms: int = 0) -> None:
        """Scalar fast SET (§5.2). ttl_ms == 0 means no TTL."""
        if not self._p:
            raise IoError("kevy: closed handle")
        kp, kn, ko = _bytes_arg(key)
        vp, vn, vo = _bytes_arg(value)
        rc = self._lib.kevy_set(self._p, kp, kn, vp, vn, ttl_ms)
        del ko, vo
        if rc < 0:
            raise IoError("kevy: kevy_set misuse or storage error")

    def subscribe(self, channel: bytes) -> "Sub":
        return self._sub(channel, pattern=False)

    def psubscribe(self, pattern: bytes) -> "Sub":
        return self._sub(pattern, pattern=True)

    def _sub(self, name: bytes, pattern: bool) -> "Sub":
        if not self._p:
            raise IoError("kevy: closed handle")
        np, nn, owner = _bytes_arg(name)
        fn = self._lib.kevy_psubscribe if pattern else self._lib.kevy_subscribe
        handle = fn(self._p, np, nn)
        del owner
        if not handle:
            raise IoError("kevy: subscribe failed")
        return Sub(handle, self._lib)


class Sub:
    """An embedded subscription over one channel or pattern (§5.2). Poll
    with ``next()``; block with ``wait(timeout_ms)``; ``close()`` once."""

    __slots__ = ("_p", "_lib")

    def __init__(self, handle: int, lib: ctypes.CDLL):
        self._p = handle
        self._lib = lib

    def next(self) -> Optional[Reply]:
        """Drain one pending frame without blocking. None when empty."""
        if not self._p:
            raise IoError("kevy: closed subscription")
        out = KevyBuf()
        rc = self._lib.kevy_sub_next(self._p, ctypes.byref(out))
        if rc < 0:
            raise IoError("kevy: subscription misuse")
        if rc == 0:
            return None
        return decode_reply(_take_buf(self._lib, out))

    def wait(self, timeout_ms: int) -> Optional[Reply]:
        """Block up to ``timeout_ms`` (0 = forever) for one frame. None on
        timeout. Parks the thread — no busy-poll."""
        if not self._p:
            raise IoError("kevy: closed subscription")
        out = KevyBuf()
        rc = self._lib.kevy_sub_wait(self._p, timeout_ms, ctypes.byref(out))
        if rc < 0:
            raise IoError("kevy: subscription closed")
        if rc == 0:
            return None
        return decode_reply(_take_buf(self._lib, out))

    def close(self) -> None:
        if self._p:
            self._lib.kevy_sub_close(self._p)
            self._p = 0

    def __del__(self):
        try:
            self.close()
        except Exception:
            pass


def open_mem() -> DB:
    """Open a pure in-memory store — nothing survives the process."""
    handle = _library().kevy_open_mem()
    if not handle:
        raise IoError("kevy: open_mem failed")
    return DB(handle)


def open_persistent(directory: str) -> DB:
    """Open a persistent store rooted at ``directory``, replaying its log on
    open (snapshot then AOF)."""
    raw = directory.encode("utf-8")
    dp, dn, owner = _bytes_arg(raw)
    handle = _library().kevy_open(dp, dn)
    del owner
    if not handle:
        raise IoError(f"kevy: open({directory!r}) failed")
    return DB(handle)

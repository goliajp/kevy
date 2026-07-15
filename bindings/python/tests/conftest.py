"""Test harness: locate + spawn a real kevy server for the remote-backend
tests, and provide a "both backends" fixture so the §6 conformance
checklist runs against embedded (mem://) and remote (kevy://) alike."""

from __future__ import annotations

import itertools
import os
import socket
import subprocess
import sys
import tempfile
import time

import pytest

# Make the in-tree package importable without an install step.
_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.abspath(os.path.join(_HERE, "..")))

import kevy  # noqa: E402

_PRIMARY = "/Users/doracawl/workspace/goliajp/kevy"
_counter = itertools.count(1)


def unique_id() -> int:
    return next(_counter)


def free_port() -> int:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def server_binary() -> str | None:
    env = os.environ.get("KEVY_SERVER_BIN")
    candidates = [env] if env else []
    repo = os.path.abspath(os.path.join(_HERE, "..", "..", ".."))
    candidates += [
        os.path.join(repo, "target", "release", "kevy"),
        os.path.join(repo, "target", "debug", "kevy"),
        os.path.join(_PRIMARY, "target", "release", "kevy"),
        os.path.join(_PRIMARY, "target", "debug", "kevy"),
    ]
    for c in candidates:
        if c and os.path.exists(c):
            return c
    return None


class SpawnedServer:
    def __init__(self, port: int, proc: subprocess.Popen, tmp: str):
        self.port = port
        self._proc = proc
        self._tmp = tmp

    @property
    def url(self) -> str:
        return f"kevy://127.0.0.1:{self.port}"

    def stop(self) -> None:
        self._proc.terminate()
        try:
            self._proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self._proc.kill()
            self._proc.wait()


def spawn_server(*extra: str, config: str | None = None) -> SpawnedServer:
    binary = server_binary()
    if binary is None:
        pytest.skip("kevy server binary not found (cargo build --release -p kevy)")
    port = free_port()
    tmp = tempfile.mkdtemp(prefix="kevy-pytest-")
    args = [binary, "--bind", "127.0.0.1", "--port", str(port), "--dir", tmp]
    if config is not None:
        cfg_path = os.path.join(tmp, "kevy.toml")
        with open(cfg_path, "w") as f:
            f.write(config)
        args += ["--config", cfg_path]
    args += list(extra)
    proc = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    srv = SpawnedServer(port, proc, tmp)
    _wait_ready(srv)
    return srv


def _wait_ready(srv: SpawnedServer) -> None:
    deadline = time.time() + 10
    while time.time() < deadline:
        if srv._proc.poll() is not None:
            pytest.skip("kevy server exited before becoming ready")
        try:
            c = kevy.connect(srv.url)
            try:
                c.ping()
                return
            finally:
                c.close()
        except kevy.KevyError:
            time.sleep(0.03)
    srv.stop()
    raise RuntimeError(f"server on port {srv.port} never became ready")


# --- shared session server for the plain both-backends tests ------------


@pytest.fixture(scope="session")
def shared_server():
    binary = server_binary()
    if binary is None:
        yield None
        return
    srv = spawn_server()
    yield srv
    srv.stop()


class Backend:
    def __init__(self, name: str, url: str):
        self.name = name
        self.url = url

    def connect(self) -> "kevy.Client":
        return kevy.connect(self.url)

    async def aconnect(self) -> "kevy.AsyncClient":
        return await kevy.AsyncClient.connect(self.url)


@pytest.fixture(params=["embedded", "remote"])
def backend(request, shared_server) -> Backend:
    if request.param == "embedded":
        return Backend("embedded", f"mem://t-{unique_id()}")
    if shared_server is None:
        pytest.skip("kevy server binary not found (cargo build --release -p kevy)")
    # Fresh state per test on the shared remote server.
    c = kevy.connect(shared_server.url)
    try:
        c.flushall()
    finally:
        c.close()
    return Backend("remote", shared_server.url)


@pytest.fixture
def emb_backend() -> Backend:
    return Backend("embedded", f"mem://t-{unique_id()}")


@pytest.fixture
def remote_server(shared_server):
    if shared_server is None:
        pytest.skip("kevy server binary not found (cargo build --release -p kevy)")
    c = kevy.connect(shared_server.url)
    try:
        c.flushall()
    finally:
        c.close()
    return shared_server

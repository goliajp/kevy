"""Reconnect / robustness (contract §6, last group)."""

import socket

import kevy
import pytest


def test_dropped_connection_surfaces_closed_or_io(remote_server):
    c = kevy.connect(remote_server.url)
    c.set("k", "v")
    # Hard-drop the underlying socket beneath the client.
    c._remote._sock.close()  # type: ignore[union-attr]
    with pytest.raises((kevy.ClosedError, kevy.IoError)):
        # a few ops, since the first write may buffer before the read fails
        for _ in range(3):
            c.get("k")
    c.close()


def test_reconnect_on_fresh_connect_resumes(remote_server):
    c1 = kevy.connect(remote_server.url)
    c1.set("persisted", "still-here")
    c1.close()  # drop
    # A fresh connect resumes commands against the same server.
    c2 = kevy.connect(remote_server.url)
    assert c2.get("persisted") == b"still-here"
    c2.set("more", "ok")
    assert c2.get("more") == b"ok"
    c2.close()

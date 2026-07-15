"""Connection & URL routing (contract §1, §6)."""

import kevy
import pytest


def test_mem_anon_isolated():
    a = kevy.connect("mem://")
    b = kevy.connect("mem://")
    a.set("k", "1")
    assert b.get("k") is None  # two anonymous mem:// are independent
    a.close()
    b.close()


def test_mem_named_shares_store():
    name = "mem://shared-store-1"
    a = kevy.connect(name)
    b = kevy.connect(name)
    a.set("k", "v")
    assert b.get("k") == b"v"  # same backing store + bus
    a.close()
    b.close()


def test_file_shares_store(tmp_path):
    url = f"file://{tmp_path}/db"
    a = kevy.connect(url)
    b = kevy.connect(url)
    a.set("k", "v")
    assert b.get("k") == b"v"
    a.close()
    b.close()


def test_tls_rejected():
    for url in ("rediss://h", "kevys://h"):
        with pytest.raises(kevy.UnsupportedError):
            kevy.connect(url)


def test_auth_rejected():
    with pytest.raises(kevy.UnsupportedError):
        kevy.connect("redis://user:pass@host")


def test_unknown_scheme():
    with pytest.raises(kevy.InvalidInputError):
        kevy.connect("weird://host")


def test_file_empty_path():
    with pytest.raises(kevy.InvalidInputError):
        kevy.connect("file://")


def test_bad_db_index():
    with pytest.raises(kevy.InvalidInputError):
        kevy.connect("kevy://127.0.0.1:6379/notanint")


def test_is_embedded_flag():
    c = kevy.connect("mem://flag")
    assert c.is_embedded is True
    c.close()

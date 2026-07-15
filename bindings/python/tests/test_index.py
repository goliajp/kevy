"""Declarative indexes IDX.* (contract §3.8, §6). Remote-only."""

import time

import kevy
from kevy import IdxType


def _wait_ready(c, name, timeout=5.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        for info in c.idx_list():
            if info.name == name.encode() and info.state == "ready":
                return
        time.sleep(0.02)
    raise AssertionError(f"index {name} never became ready")


def test_range_paging_and_eq(remote_server):
    c = kevy.connect(remote_server.url)
    c.idx_create_range("byage", "user:", "age", IdxType.I64)
    for i, age in enumerate(["21", "22", "23", "24", "25"]):
        c.hset(f"user:{chr(ord('a') + i)}", "age", age)
    _wait_ready(c, "byage")

    infos = c.idx_list()
    assert any(i.name == b"byage" and i.kind == "range" for i in infos)  # unknown labels skipped

    # LIMIT 2 pages through 5 rows; cursor advances, ends at None.
    seen = 0
    cursor = None
    while True:
        page = c.idx_query_range("byage", "0", "100", 2, cursor)
        seen += len(page.rows)
        if page.cursor is None:
            break
        cursor = page.cursor
        assert seen <= 10
    assert seen == 5

    page = c.idx_query_eq("byage", "23", 10)
    assert len(page.rows) == 1 and page.rows[0].value == b"23"

    assert c.idx_drop("byage") is True
    assert c.idx_drop("byage") is False
    c.close()


def test_raw_query_passthrough(remote_server):
    c = kevy.connect(remote_server.url)
    c.idx_create_range("byid", "item:", "id", IdxType.I64)
    c.hset("item:1", "id", "1")
    _wait_ready(c, "byid")
    # raw COUNT via the escape hatch reaches server capability directly
    r = c.do(b"IDX.COUNT", b"byid", b"RANGE", b"0", b"100")
    assert r.kind.name in ("INT", "SIMPLE", "BULK")
    c.close()


def test_embedded_unsupported():
    c = kevy.connect("mem://idx-bus")
    for call in (
        lambda: c.idx_list(),
        lambda: c.idx_create_range("i", "u:", "age", IdxType.I64),
        lambda: c.idx_query_range("i", "0", "1", 10),
    ):
        try:
            call()
            assert False, "expected UnsupportedError"
        except kevy.UnsupportedError:
            pass
    c.close()

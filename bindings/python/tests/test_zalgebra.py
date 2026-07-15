"""Sorted-set algebra: ZINTERSTORE / ZUNIONSTORE / ZINTERCARD (§3.6, §6)."""

import kevy
import pytest
from kevy import ZAggregate


def _seed(c):
    c.zadd("a", (1.0, "x"), (2.0, "y"), (3.0, "z"))
    c.zadd("b", (10.0, "y"), (20.0, "z"), (30.0, "w"))


def test_interstore_unweighted(backend):
    c = backend.connect()
    _seed(c)
    assert c.zinterstore("dest", "a", "b") == 2  # {y, z}
    assert c.zscore("dest", "y") == 12.0  # SUM by default
    c.close()


def test_unionstore_weights_aggregate(backend):
    c = backend.connect()
    _seed(c)
    n = c.zunionstore_with("dest", ["a", "b"], weights=[1.0, 2.0], aggregate=ZAggregate.MAX)
    assert n == 4  # {x, y, z, w}
    # y: max(1*1, 10*2) = 20
    assert c.zscore("dest", "y") == 20.0
    c.close()


def test_intercard_with_and_without_limit(backend):
    c = backend.connect()
    _seed(c)
    assert c.zintercard(["a", "b"]) == 2
    assert c.zintercard(["a", "b"], limit=1) == 1
    c.close()


def test_empty_keys_invalid(backend):
    c = backend.connect()
    with pytest.raises(kevy.InvalidInputError):
        c.zintercard([])
    with pytest.raises(kevy.InvalidInputError):
        c.zinterstore_with("d", [])
    c.close()


def test_weights_arity_invalid(backend):
    c = backend.connect()
    _seed(c)
    with pytest.raises(kevy.InvalidInputError):
        c.zunionstore_with("d", ["a", "b"], weights=[1.0])  # one weight, two keys
    c.close()

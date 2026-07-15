"""Hash-field TTL: HEXPIRE / HPEXPIRE / HPERSIST / HTTL / HPTTL (§3.7, §6)."""

import kevy
import pytest
from kevy import HExpireCond


def test_hexpire_codes_and_ttls(backend):
    c = backend.connect()
    c.hset("h", "f1", "v1", "f2", "v2")
    codes = c.hexpire("h", ["f1", "missing"], 100.0)
    assert codes[0] == 1  # deadline set
    assert codes[1] == -2  # field missing
    # httl in seconds, hpttl in ms
    secs = c.httl("h", "f1")
    assert 0 < secs[0] <= 100
    ms = c.hpttl("h", "f1")
    assert 0 < ms[0] <= 100_000
    c.close()


def test_hpexpire_and_hpersist(backend):
    c = backend.connect()
    c.hset("h", "f1", "v1")
    assert c.hpexpire("h", ["f1"], 50_000.0)[0] == 1
    assert c.httl("h", "f1")[0] > 0
    assert c.hpersist("h", "f1")[0] == 1  # cleared
    assert c.httl("h", "f1")[0] == -1  # no TTL now
    c.close()


def test_hexpire_condition(backend):
    c = backend.connect()
    c.hset("h", "f1", "v1")
    c.hexpire("h", ["f1"], 100.0)
    # NX: set only when no TTL — f1 already has one → 0 (condition not met)
    assert c.hexpire("h", ["f1"], 200.0, HExpireCond.NX)[0] == 0
    c.close()


def test_empty_fields_invalid(backend):
    c = backend.connect()
    with pytest.raises(kevy.InvalidInputError):
        c.hexpire("h", [], 100.0)
    with pytest.raises(kevy.InvalidInputError):
        c.httl("h")
    c.close()

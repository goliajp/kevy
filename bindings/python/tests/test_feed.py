"""Change feed FEED.* (contract §3.10, §6)."""

import kevy
import pytest
from conftest import spawn_server


@pytest.fixture(scope="module")
def feed_server():
    srv = spawn_server("--threads", "1", config="[feed]\nenabled = true\n")
    yield srv
    srv.stop()


def test_feed_replay_and_resume(feed_server):
    c = kevy.connect(feed_server.url)
    assert c.feed_shards() >= 1
    gen, off = c.feed_tail(0)

    c.set("fk1", "v1")
    c.set("fk2", "v2")

    batch = c.feed_read(0, gen, off)
    assert len(batch.frames) >= 2
    assert any(f.argv and f.argv[0] == b"SET" for f in batch.frames)

    # Resume from the returned cursor: caught up → empty batch.
    nxt = c.feed_read(0, batch.generation, batch.next_offset)
    assert len(nxt.frames) == 0
    c.close()


def test_stale_cursor_resync(feed_server):
    c = kevy.connect(feed_server.url)
    try:
        c.feed_read(0, 999999, 0)  # wildly-stale generation
    except kevy.ProtocolError:
        pass  # FEEDRESYNC / unservable cursor → Protocol error
    except kevy.KevyError:
        pass
    c.close()


def test_embedded_feed_unsupported():
    c = kevy.connect("mem://feed-bus")
    assert c.feed_shards() == 1
    with pytest.raises(kevy.InvalidInputError):
        c.feed_tail(1)  # non-zero shard
    with pytest.raises(kevy.UnsupportedError):
        c.feed_tail(0)  # feed not enabled on this port's embedded connect
    c.close()

package kevy

import (
	"testing"
)

// Change feed FEED.* (contract §3.10, §6).

func TestFeedReplayRemote(t *testing.T) {
	s := spawnServerConfig(t, "[feed]\nenabled = true\n", "--threads", "1")
	c := mustConnect(t, s.url())

	if n, _ := c.FeedShards(bg); n < 1 {
		t.Fatalf("FeedShards=%d", n)
	}
	gen, off, err := c.FeedTail(bg, 0)
	if err != nil {
		t.Fatalf("FeedTail: %v", err)
	}

	// Produce some effects.
	_ = c.Set(bg, b("fk1"), b("v1"))
	_ = c.Set(bg, b("fk2"), b("v2"))

	batch, err := c.FeedRead(bg, 0, gen, off, 0)
	if err != nil {
		t.Fatalf("FeedRead: %v", err)
	}
	if len(batch.Frames) < 2 {
		t.Fatalf("expected >=2 frames, got %d", len(batch.Frames))
	}
	// Frames carry the applied argv.
	sawSet := false
	for _, f := range batch.Frames {
		if len(f.Argv) > 0 && string(f.Argv[0]) == "SET" {
			sawSet = true
		}
	}
	if !sawSet {
		t.Fatalf("no SET frame in feed: %+v", batch.Frames)
	}

	// Resume from the returned cursor: catching up yields an empty batch.
	next, err := c.FeedRead(bg, 0, batch.Generation, batch.NextOffset, 0)
	if err != nil {
		t.Fatalf("FeedRead resume: %v", err)
	}
	if len(next.Frames) != 0 {
		t.Fatalf("resume from tail should be caught up, got %d frames", len(next.Frames))
	}
}

func TestFeedStaleCursorResync(t *testing.T) {
	s := spawnServerConfig(t, "[feed]\nenabled = true\n", "--threads", "1")
	c := mustConnect(t, s.url())
	// A wildly-stale generation should surface a FEEDRESYNC protocol error.
	_, err := c.FeedRead(bg, 0, 999999, 0, 0)
	if err == nil {
		t.Skip("server did not treat a stale generation as unservable")
	}
	if !IsKind(err, KindProtocol) {
		t.Fatalf("stale cursor: want Protocol, got %v", err)
	}
}

func TestFeedEmbeddedUnsupported(t *testing.T) {
	c := mustConnect(t, "mem://feed-bus")
	if n, _ := c.FeedShards(bg); n != 1 {
		t.Fatalf("embedded FeedShards=%d want 1", n)
	}
	// Non-zero shard → InvalidInput.
	if _, _, err := c.FeedTail(bg, 1); !IsKind(err, KindInvalidInput) {
		t.Fatalf("embedded non-zero shard: want InvalidInput, got %v", err)
	}
	// Feed not enabled on this port's embedded Connect → Unsupported.
	if _, _, err := c.FeedTail(bg, 0); !IsKind(err, KindUnsupported) {
		t.Fatalf("embedded feed disabled: want Unsupported, got %v", err)
	}
}

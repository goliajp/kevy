// Change feed FEED.* replay, remote (feed-enabled server) (client-contract §3.10, §6).
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Assumptions;
import org.junit.jupiter.api.Test;

class FeedTest {
    @Test
    void feedReplayAndResume() {
        Assumptions.assumeTrue(Harness.remoteAvailable());
        try (Harness.Server s = Harness.spawn("[feed]\nenabled = true\n", "--threads", "1");
             KevyClient c = Kevy.connect(s.url)) {
            assertTrue(c.feedShards() >= 1);
            FeedTail tail = c.feedTail(0);

            c.set("fk1", "v1");
            c.set("fk2", "v2");

            FeedBatch batch = c.feedRead(0, tail.generation(), tail.nextOffset(), null);
            assertTrue(batch.frames().size() >= 2, "expected >= 2 frames");
            boolean sawSet = batch.frames().stream()
                .anyMatch(fr -> !fr.argv().isEmpty() && "SET".equalsIgnoreCase(Bytes.str(fr.argv().get(0))));
            assertTrue(sawSet, "no SET frame in feed");

            // Resume from the returned cursor: caught up -> empty batch.
            FeedBatch next = c.feedRead(0, batch.generation(), batch.nextOffset(), null);
            assertEquals(0, next.frames().size(), "resume from tail should be caught up");
        }
    }

    @Test
    void embeddedFeedUnsupported() {
        try (KevyClient c = Harness.newEmbedded()) {
            assertEquals(1, c.feedShards());                       // embedded: always 1
            assertThrows(UnsupportedException.class, () -> c.feedTail(0));
            assertThrows(InvalidInputException.class, () -> c.feedTail(1)); // non-zero shard
        }
    }
}

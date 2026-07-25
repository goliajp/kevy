// Pipeline (non-atomic batching), remote-only (client-contract §3.13, §6).
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import java.util.List;
import org.junit.jupiter.api.Assumptions;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

class PipelineTest {
    @BeforeEach
    void requireRemote() {
        Assumptions.assumeTrue(Harness.remoteAvailable());
    }

    @Test
    void repliesInOrder() {
        try (KevyClient c = Harness.newRemote()) {
            List<Reply> r = c.pipeline(p -> p.cmd("SET", "k", "v").cmd("GET", "k").cmd("INCR", "n"));
            assertEquals(3, r.size());
            assertInstanceOf(Reply.Simple.class, r.get(0));
            assertEquals("v", Bytes.str(r.get(1).payload()));
            assertEquals(1, ((Reply.Int) r.get(2)).value());
        }
    }

    @Test
    void perCommandErrorLandsInline() {
        try (KevyClient c = Harness.newRemote()) {
            List<Reply> r = c.pipeline(p -> p.cmd("SET", "k", "v").cmd("LPUSH", "k", "x").cmd("GET", "k"));
            assertEquals(3, r.size());
            assertTrue(r.get(1).isError(), "per-command -ERR is inline, not thrown");
            assertEquals("v", Bytes.str(r.get(2).payload()), "batch not aborted by the inline error");
        }
    }

    @Test
    void emptyBatchNoWire() {
        try (KevyClient c = Harness.newRemote()) {
            assertTrue(c.pipeline(p -> { }).isEmpty());
        }
    }

    @Test
    void emptyArgvPoisonsBatch() {
        try (KevyClient c = Harness.newRemote()) {
            assertThrows(InvalidInputException.class, () -> c.pipeline(p -> p.cmd(new byte[0][])));
        }
    }
}

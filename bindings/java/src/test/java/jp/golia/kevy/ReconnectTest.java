// Reconnect / robustness (client-contract §6): a dropped remote connection
// surfaces Closed/Io, and a fresh connect resumes commands.
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Assumptions;
import org.junit.jupiter.api.Test;

class ReconnectTest {
    @Test
    void dropSurfacesThenReconnects() {
        Assumptions.assumeTrue(Harness.remoteAvailable());
        Harness.Server s1 = Harness.spawn(null);
        KevyClient c = Kevy.connect(s1.url);
        c.set("k", "v");
        s1.close(); // kill the server under the client

        KevyException dropped = null;
        for (int i = 0; i < 30 && dropped == null; i++) {
            try {
                c.ping();
                Harness.sleep(100);
            } catch (KevyException e) {
                dropped = e;
            }
        }
        c.close();
        assertNotNull(dropped, "a dropped connection should surface an error");
        assertTrue(dropped.kind() == ErrorKind.CLOSED || dropped.kind() == ErrorKind.IO,
            "drop should be Closed or Io, got " + dropped.kind());

        // Reconnect on a fresh connect and resume.
        try (Harness.Server s2 = Harness.spawn(null);
             KevyClient c2 = Kevy.connect(s2.url)) {
            c2.set("k", "again");
            assertEquals("again", c2.get("k").map(Bytes::str).orElse(null));
        }
    }
}

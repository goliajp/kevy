// Declarative secondary indexes IDX.* query, remote-only (client-contract §3.8, §6).
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Assumptions;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

class IndexTest {
    @BeforeEach
    void requireRemote() {
        Assumptions.assumeTrue(Harness.remoteAvailable());
    }

    private static void waitReady(KevyClient c, String name) {
        long deadline = System.nanoTime() + 5_000_000_000L;
        while (System.nanoTime() < deadline) {
            for (IdxInfo in : c.idxList()) {
                if (name.equals(in.nameStr()) && "ready".equals(in.state())) return;
            }
            Harness.sleep(20);
        }
        fail("index " + name + " never became ready");
    }

    @Test
    void rangePagingAndEq() {
        try (KevyClient c = Harness.newRemote()) {
            c.idxCreateRange("byage", "user:", "age", IdxType.I64);
            String[] ages = {"21", "22", "23", "24", "25"};
            for (int i = 0; i < ages.length; i++) {
                c.hset("user:" + (char) ('a' + i), "age", ages[i]);
            }
            waitReady(c, "byage");

            // idx_list parses IdxInfo (kind=range).
            boolean found = c.idxList().stream().anyMatch(in -> "byage".equals(in.nameStr()) && "range".equals(in.kind()));
            assertTrue(found, "byage not in IDX.LIST");

            // Range query paging: LIMIT 2 across 5 rows [21..25], cursor ends at null.
            int seen = 0;
            byte[] cursor = null;
            for (int guard = 0; guard < 10; guard++) {
                IdxPage page = c.idxQueryRange("byage", Bytes.of("0"), Bytes.of("100"), 2, cursor);
                seen += page.rows().size();
                if (page.done()) break;
                cursor = page.cursor();
            }
            assertEquals(5, seen, "range paging row count");

            // EQ point lookup.
            IdxPage eq = c.idxQueryEq("byage", Bytes.of("23"), 10);
            assertEquals(1, eq.rows().size());
            assertEquals("23", eq.rows().get(0).valueStr());

            assertTrue(c.idxDrop("byage"));
            assertFalse(c.idxDrop("byage")); // already gone
        }
    }
}

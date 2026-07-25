// Transactions MULTI/EXEC/DISCARD + WATCH, remote-only (client-contract §3.12, §6).
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import java.util.List;
import java.util.Optional;
import org.junit.jupiter.api.Assumptions;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

class TransactionTest {
    @BeforeEach
    void requireRemote() {
        Assumptions.assumeTrue(Harness.remoteAvailable());
    }

    @Test
    void queueExecOrder() {
        try (KevyClient c = Harness.newRemote()) {
            Transaction tx = c.multi();
            tx.set(Bytes.of("k"), Bytes.of("v")).incr(Bytes.of("n"));
            List<Reply> replies = tx.exec();
            assertEquals(2, replies.size());
            assertInstanceOf(Reply.Simple.class, replies.get(0));
            assertEquals(1, ((Reply.Int) replies.get(1)).value());
        }
    }

    @Test
    void typedCursorAndArityGate() {
        try (KevyClient c = Harness.newRemote()) {
            Transaction tx = c.multi();
            tx.set(Bytes.of("k"), Bytes.of("v")).incr(Bytes.of("n"));
            TransactionReplies rep = tx.execTyped();
            rep.nextOk();
            assertEquals(1, rep.nextInt());
            rep.expectEmpty(); // arity gate: nothing left
        }
    }

    @Test
    void watchAbortReturnsEmpty() {
        try (KevyClient c1 = Harness.newRemote();
             KevyClient c2 = Kevy.connect(Harness.shared().url)) {
            c1.set("wk", "0");
            c1.watch(Bytes.of("wk"));
            Transaction tx = c1.multi();
            tx.set(Bytes.of("wk"), Bytes.of("new"));
            c2.set("wk", "changed"); // concurrent modify aborts the txn
            Optional<List<Reply>> res = tx.execWatched();
            assertTrue(res.isEmpty(), "WATCH violation should abort EXEC");
        }
    }

    @Test
    void abandonSendsImplicitDiscard() {
        try (KevyClient c = Harness.newRemote()) {
            try (Transaction tx = c.multi()) {
                tx.set(Bytes.of("k"), Bytes.of("v")); // never exec/discard -> close() discards
            }
            c.ping(); // socket not stuck in MULTI mode
            assertTrue(c.get("k").isEmpty(), "queued command must not have applied");
        }
    }
}

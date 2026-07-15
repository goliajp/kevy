// Pub/sub round-trip (client-contract §3.11, §6). Embedded named bus +
// remote; anonymous mem:// rejected; read-timeout bounds a blocking recv.
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import java.time.Duration;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Assumptions;
import org.junit.jupiter.api.Test;

class PubsubTest {
    private static final AtomicInteger SEQ = new AtomicInteger();

    @Test
    void anonymousMemRejected() {
        assertThrows(UnsupportedException.class, () -> Subscriber.connect("mem://"));
    }

    @Test
    void embeddedNamedBusDelivers() {
        String url = "mem://pubsub-" + SEQ.incrementAndGet();
        try (Subscriber sub = Subscriber.connectChannels(url, Bytes.of("ch"));
             KevyClient pub = Kevy.connect(url)) {
            sub.setReadTimeout(Duration.ofSeconds(2));
            assertEquals(1, pub.publish("ch", "hello")); // named bus reports a real count
            PubsubEvent m = sub.recvMessage();
            assertEquals("ch", m.channelStr());
            assertEquals("hello", m.payloadStr());
        }
    }

    @Test
    void embeddedReadTimeoutBounds() {
        String url = "mem://pubsub-to-" + SEQ.incrementAndGet();
        try (Subscriber sub = Subscriber.connectChannels(url, Bytes.of("ch"))) {
            // Drain the subscribe ack first; with no publishes, the next recv times out.
            sub.setReadTimeout(Duration.ofMillis(500));
            assertEquals(PubsubEvent.Kind.SUBSCRIBE, sub.recv().kind());
            sub.setReadTimeout(Duration.ofMillis(200));
            assertThrows(TimedOutException.class, sub::recv);
        }
    }

    @Test
    void remoteMessageAndPattern() {
        Assumptions.assumeTrue(Harness.remoteAvailable());
        String base = Harness.shared().url;
        try (Subscriber sub = Subscriber.connect(base);
             KevyClient pub = Kevy.connect(base)) {
            sub.hello3(); // RESP3 push frames
            sub.subscribe(Bytes.of("news"));
            sub.psubscribe(Bytes.of("ne*"));
            Harness.sleep(150); // let the server register the subscriptions
            pub.publish("news", "hi");
            // one direct Message + one Pmessage delivery expected
            boolean sawMessage = false, sawPmessage = false;
            sub.setReadTimeout(Duration.ofSeconds(2));
            for (int i = 0; i < 6 && !(sawMessage && sawPmessage); i++) {
                PubsubEvent ev = sub.recv();
                if (ev.kind() == PubsubEvent.Kind.MESSAGE) sawMessage = true;
                if (ev.kind() == PubsubEvent.Kind.PMESSAGE) sawPmessage = true;
            }
            assertTrue(sawMessage, "expected a Message delivery");
            assertTrue(sawPmessage, "expected a Pmessage delivery");
        }
    }
}

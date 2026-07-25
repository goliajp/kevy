// BlockingOps — blocking pops (client-contract §3.14). Remote parks the
// connection server-side (a real block). The embedded C-ABI door has no
// blocking-pop symbol, so embedded emulates it by polling the non-blocking
// pop on a short interval until a hit or the deadline — observably the same,
// only a bounded poll latency differs. timeout=null waits forever (wire 0);
// a zero timeout is ambiguous → InvalidInput; empty keys → InvalidInput.
package jp.golia.kevy;

import java.time.Duration;
import java.util.List;
import java.util.Optional;

final class BlockingOps {
    private BlockingOps() {}

    private static final long POLL_MS = 5;

    static Optional<KeyValue> blpop(Backend b, byte[][] keys, Duration timeout) {
        return listPop(b, "BLPOP", "LPOP", keys, timeout);
    }

    static Optional<KeyValue> brpop(Backend b, byte[][] keys, Duration timeout) {
        return listPop(b, "BRPOP", "RPOP", keys, timeout);
    }

    static Optional<ZPopHit> bzpopmin(Backend b, byte[][] keys, Duration timeout) {
        validate(keys, timeout);
        if (!b.embedded()) return remoteZ(b, keys, timeout);
        long deadline = deadline(timeout);
        while (true) {
            for (byte[] key : keys) {
                Optional<ZPopHit> hit = embeddedZ(b, key);
                if (hit.isPresent()) return hit;
            }
            if (!sleepUntil(deadline)) return Optional.empty();
        }
    }

    private static Optional<KeyValue> listPop(Backend b, String bverb, String verb, byte[][] keys, Duration timeout) {
        validate(keys, timeout);
        if (!b.embedded()) return remoteKV(b, bverb, keys, timeout);
        long deadline = deadline(timeout);
        while (true) {
            for (byte[] key : keys) {
                List<byte[]> got = Decode.bulks(b.exec(Argv.cmd(verb).add(key).addLong(1).list()));
                if (!got.isEmpty() && got.get(0) != null) return Optional.of(new KeyValue(key, got.get(0)));
            }
            if (!sleepUntil(deadline)) return Optional.empty();
        }
    }

    private static Optional<KeyValue> remoteKV(Backend b, String verb, byte[][] keys, Duration timeout) {
        Argv a = Argv.cmd(verb).addAll(keys).add(secs(timeout));
        List<byte[]> items = Decode.bulks(b.exec(a.list()));
        if (items.size() < 2) return Optional.empty();
        return Optional.of(new KeyValue(items.get(0), items.get(1)));
    }

    private static Optional<ZPopHit> remoteZ(Backend b, byte[][] keys, Duration timeout) {
        Argv a = Argv.cmd("BZPOPMIN").addAll(keys).add(secs(timeout));
        List<byte[]> items = Decode.bulks(b.exec(a.list()));
        if (items.size() < 3) return Optional.empty();
        return Optional.of(new ZPopHit(items.get(0), items.get(1), Decode.parseFloat(items.get(2))));
    }

    private static Optional<ZPopHit> embeddedZ(Backend b, byte[] key) {
        List<byte[]> got = Decode.bulks(b.exec(Argv.cmd("ZPOPMIN").add(key).list()));
        if (got.size() < 2) return Optional.empty();
        return Optional.of(new ZPopHit(key, got.get(0), Decode.parseFloat(got.get(1))));
    }

    private static void validate(byte[][] keys, Duration timeout) {
        if (keys.length == 0) throw new InvalidInputException("blocking pop needs at least one key");
        if (timeout != null && timeout.isZero()) {
            throw new InvalidInputException("zero timeout is ambiguous (wire 0 means forever)");
        }
    }

    // Fractional-seconds wire form; null → "0" (forever).
    private static byte[] secs(Duration timeout) {
        if (timeout == null) return Bytes.of("0");
        double s = timeout.toNanos() / 1_000_000_000.0;
        return Bytes.of(Double.toString(s));
    }

    private static long deadline(Duration timeout) {
        return timeout == null ? Long.MAX_VALUE : System.nanoTime() + timeout.toNanos();
    }

    /** Sleep one poll tick; false once the deadline has passed. */
    private static boolean sleepUntil(long deadline) {
        if (deadline != Long.MAX_VALUE && System.nanoTime() >= deadline) return false;
        try {
            Thread.sleep(POLL_MS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return false;
        }
        return true;
    }
}

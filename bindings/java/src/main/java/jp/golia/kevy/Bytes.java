// Bytes — argv-building helpers. All keys/values are binary-safe byte strings
// (client-contract §7): the client never assumes UTF-8 for user data. Convenience
// overloads accept String (encoded UTF-8) alongside byte[]; integers/floats use
// the Redis wire text forms.
package jp.golia.kevy;

import java.nio.charset.StandardCharsets;

public final class Bytes {
    private Bytes() {}

    public static byte[] of(String s) {
        return s.getBytes(StandardCharsets.UTF_8);
    }

    public static byte[] of(byte[] b) {
        return b;
    }

    public static String str(byte[] b) {
        return b == null ? null : new String(b, StandardCharsets.UTF_8);
    }

    /** Duration → milliseconds, clamped to Long.MAX_VALUE (client-contract §3.1). */
    static long toMs(java.time.Duration d) {
        try {
            long ms = d.toMillis();
            return ms < 0 ? 0 : ms;
        } catch (ArithmeticException overflow) {
            return Long.MAX_VALUE;
        }
    }

    /** Base-10 integer as ASCII bytes (INCR deltas, counts, LIMIT n, …). */
    static byte[] ofLong(long n) {
        return Long.toString(n).getBytes(StandardCharsets.US_ASCII);
    }

    /**
     * A float in Redis' shortest round-tripping form. `inf`/`-inf`/`nan`
     * spellings match the server so ZADD/scores agree.
     */
    static byte[] ofDouble(double d) {
        String s;
        if (d == Double.POSITIVE_INFINITY) s = "inf";
        else if (d == Double.NEGATIVE_INFINITY) s = "-inf";
        else if (Double.isNaN(d)) s = "nan";
        else if (d == Math.floor(d) && !Double.isInfinite(d) && Math.abs(d) < 1e15) {
            s = Long.toString((long) d);
        } else {
            s = Double.toString(d);
        }
        return s.getBytes(StandardCharsets.US_ASCII);
    }
}

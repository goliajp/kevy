// Argv — a tiny fluent builder for a command's binary-safe argument vector.
// Keeps the family ops readable without stringly-typed concatenation.
package jp.golia.kevy;

import java.util.ArrayList;
import java.util.List;

final class Argv {
    private final List<byte[]> parts = new ArrayList<>();

    static Argv cmd(String verb) {
        return new Argv().add(verb);
    }

    Argv add(String s) {
        parts.add(Bytes.of(s));
        return this;
    }

    Argv add(byte[] b) {
        parts.add(b);
        return this;
    }

    Argv addLong(long n) {
        parts.add(Bytes.ofLong(n));
        return this;
    }

    Argv addDouble(double d) {
        parts.add(Bytes.ofDouble(d));
        return this;
    }

    Argv addAll(byte[][] xs) {
        for (byte[] x : xs) parts.add(x);
        return this;
    }

    Argv addAll(List<byte[]> xs) {
        parts.addAll(xs);
        return this;
    }

    List<byte[]> list() {
        return parts;
    }
}

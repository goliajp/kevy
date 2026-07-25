// HashTtlOps — hash-field TTL, the Redis 7.4 shape (client-contract §3.7).
// hexpire is whole-second precision (ttl truncated to secs); hpexpire is
// milliseconds. Replies come back in request order as per-field i8 codes
// (returned here as long[]). Empty fields → InvalidInput. Both backends.
package jp.golia.kevy;

import java.time.Duration;

final class HashTtlOps {
    private HashTtlOps() {}

    static long[] hexpire(Backend b, byte[] key, byte[][] fields, Duration ttl, HExpireCond cond) {
        return expire(b, "HEXPIRE", key, fields, ttl.toSeconds(), cond);
    }

    static long[] hpexpire(Backend b, byte[] key, byte[][] fields, Duration ttl, HExpireCond cond) {
        return expire(b, "HPEXPIRE", key, fields, Bytes.toMs(ttl), cond);
    }

    private static long[] expire(Backend b, String verb, byte[] key, byte[][] fields, long amount, HExpireCond cond) {
        requireFields(fields);
        Argv a = Argv.cmd(verb).add(key).addLong(amount);
        if (cond != null && cond.wire() != null) a.add(cond.wire());
        a.add("FIELDS").addLong(fields.length).addAll(fields);
        return Decode.intList(b.exec(a.list()));
    }

    static long[] hpersist(Backend b, byte[] key, byte[][] fields) {
        return fieldsOnly(b, "HPERSIST", key, fields);
    }

    static long[] httl(Backend b, byte[] key, byte[][] fields) {
        return fieldsOnly(b, "HTTL", key, fields);
    }

    static long[] hpttl(Backend b, byte[] key, byte[][] fields) {
        return fieldsOnly(b, "HPTTL", key, fields);
    }

    private static long[] fieldsOnly(Backend b, String verb, byte[] key, byte[][] fields) {
        requireFields(fields);
        Argv a = Argv.cmd(verb).add(key).add("FIELDS").addLong(fields.length).addAll(fields);
        return Decode.intList(b.exec(a.list()));
    }

    private static void requireFields(byte[][] fields) {
        if (fields.length == 0) throw new InvalidInputException("needs at least one field");
    }
}

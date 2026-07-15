// CollOps — the hash / list / set / sorted-set families (client-contract
// §3.2–3.5). Set combine (SINTER/SUNION/SDIFF) has no embedded server op, so
// on the embedded backend it is computed client-side over SMEMBERS snapshots
// (result order unspecified, per §3.4).
package jp.golia.kevy;

import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Optional;
import java.util.OptionalDouble;

final class CollOps {
    private CollOps() {}

    // ---- Hash (§3.2) ----
    static long hset(Backend b, byte[] key, List<byte[]> fieldValueFlat) {
        return Decode.intVal(b.exec(Argv.cmd("HSET").add(key).addAll(fieldValueFlat).list()));
    }

    static Optional<byte[]> hget(Backend b, byte[] key, byte[] field) {
        return Optional.ofNullable(Decode.optBulk(b.exec(Argv.cmd("HGET").add(key).add(field).list())));
    }

    static long hdel(Backend b, byte[] key, byte[]... fields) {
        return Decode.intVal(b.exec(Argv.cmd("HDEL").add(key).addAll(fields).list()));
    }

    static long hlen(Backend b, byte[] key) {
        return Decode.intVal(b.exec(Argv.cmd("HLEN").add(key).list()));
    }

    static List<byte[]> hgetall(Backend b, byte[] key) {
        return Decode.flatMap(b.exec(Argv.cmd("HGETALL").add(key).list()));
    }

    static List<byte[]> hkeys(Backend b, byte[] key) {
        return Decode.bulks(b.exec(Argv.cmd("HKEYS").add(key).list()));
    }

    static List<byte[]> hvals(Backend b, byte[] key) {
        return Decode.bulks(b.exec(Argv.cmd("HVALS").add(key).list()));
    }

    // ---- List (§3.3) ----
    static long lpush(Backend b, byte[] key, byte[]... values) {
        return Decode.intVal(b.exec(Argv.cmd("LPUSH").add(key).addAll(values).list()));
    }

    static long rpush(Backend b, byte[] key, byte[]... values) {
        return Decode.intVal(b.exec(Argv.cmd("RPUSH").add(key).addAll(values).list()));
    }

    static List<byte[]> lpop(Backend b, byte[] key, long count) {
        return Decode.bulks(b.exec(Argv.cmd("LPOP").add(key).addLong(count).list()));
    }

    static List<byte[]> rpop(Backend b, byte[] key, long count) {
        return Decode.bulks(b.exec(Argv.cmd("RPOP").add(key).addLong(count).list()));
    }

    static long llen(Backend b, byte[] key) {
        return Decode.intVal(b.exec(Argv.cmd("LLEN").add(key).list()));
    }

    static List<byte[]> lrange(Backend b, byte[] key, long start, long stop) {
        return Decode.bulks(b.exec(Argv.cmd("LRANGE").add(key).addLong(start).addLong(stop).list()));
    }

    // ---- Set (§3.4) ----
    static long sadd(Backend b, byte[] key, byte[]... members) {
        return Decode.intVal(b.exec(Argv.cmd("SADD").add(key).addAll(members).list()));
    }

    static long srem(Backend b, byte[] key, byte[]... members) {
        return Decode.intVal(b.exec(Argv.cmd("SREM").add(key).addAll(members).list()));
    }

    static List<byte[]> smembers(Backend b, byte[] key) {
        return Decode.bulks(b.exec(Argv.cmd("SMEMBERS").add(key).list()));
    }

    static long scard(Backend b, byte[] key) {
        return Decode.intVal(b.exec(Argv.cmd("SCARD").add(key).list()));
    }

    static boolean sismember(Backend b, byte[] key, byte[] member) {
        return Decode.bool(b.exec(Argv.cmd("SISMEMBER").add(key).add(member).list()));
    }

    static List<byte[]> sinter(Backend b, byte[]... keys) {
        return b.embedded() ? combine(b, Combine.INTER, keys)
            : Decode.bulks(b.exec(Argv.cmd("SINTER").addAll(keys).list()));
    }

    static List<byte[]> sunion(Backend b, byte[]... keys) {
        return b.embedded() ? combine(b, Combine.UNION, keys)
            : Decode.bulks(b.exec(Argv.cmd("SUNION").addAll(keys).list()));
    }

    static List<byte[]> sdiff(Backend b, byte[]... keys) {
        return b.embedded() ? combine(b, Combine.DIFF, keys)
            : Decode.bulks(b.exec(Argv.cmd("SDIFF").addAll(keys).list()));
    }

    private enum Combine { INTER, UNION, DIFF }

    private static List<byte[]> combine(Backend b, Combine op, byte[][] keys) {
        if (keys.length == 0) return new ArrayList<>();
        LinkedHashSet<ByteBuffer> acc = new LinkedHashSet<>();
        for (byte[] m : smembers(b, keys[0])) acc.add(ByteBuffer.wrap(m));
        for (int i = 1; i < keys.length; i++) {
            LinkedHashSet<ByteBuffer> other = new LinkedHashSet<>();
            for (byte[] m : smembers(b, keys[i])) other.add(ByteBuffer.wrap(m));
            switch (op) {
                case INTER -> acc.retainAll(other);
                case UNION -> acc.addAll(other);
                case DIFF -> acc.removeAll(other);
            }
        }
        List<byte[]> out = new ArrayList<>(acc.size());
        for (ByteBuffer bb : acc) out.add(bb.array());
        return out;
    }

    // ---- Sorted set (§3.5) ----
    static long zadd(Backend b, byte[] key, List<ZMember> members) {
        Argv a = Argv.cmd("ZADD").add(key);
        for (ZMember m : members) a.addDouble(m.score()).add(m.member());
        return Decode.intVal(b.exec(a.list()));
    }

    static long zrem(Backend b, byte[] key, byte[]... members) {
        return Decode.intVal(b.exec(Argv.cmd("ZREM").add(key).addAll(members).list()));
    }

    static OptionalDouble zscore(Backend b, byte[] key, byte[] member) {
        Double d = Decode.optFloat(b.exec(Argv.cmd("ZSCORE").add(key).add(member).list()));
        return d == null ? OptionalDouble.empty() : OptionalDouble.of(d);
    }

    static long zcard(Backend b, byte[] key) {
        return Decode.intVal(b.exec(Argv.cmd("ZCARD").add(key).list()));
    }

    static List<byte[]> zrange(Backend b, byte[] key, long start, long stop) {
        return Decode.bulks(b.exec(Argv.cmd("ZRANGE").add(key).addLong(start).addLong(stop).list()));
    }
}

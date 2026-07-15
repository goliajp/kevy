// CoreOps — the core string / generic-key family (client-contract §3.1). Each
// method builds argv and runs it through the backend, then decodes. Shared by
// the blocking and async faces (the async face delegates to the blocking
// methods, so argv/decoding live here once).
package jp.golia.kevy;

import java.time.Duration;
import java.util.List;
import java.util.Optional;

final class CoreOps {
    private CoreOps() {}

    static void ping(Backend b) {
        Decode.ok(b.exec(Argv.cmd("PING").list()));
    }

    static void set(Backend b, byte[] key, byte[] value) {
        Decode.ok(b.exec(Argv.cmd("SET").add(key).add(value).list()));
    }

    static Optional<byte[]> get(Backend b, byte[] key) {
        return Optional.ofNullable(b.fastGet(key));
    }

    static long del(Backend b, byte[]... keys) {
        return Decode.intVal(b.exec(Argv.cmd("DEL").addAll(keys).list()));
    }

    static long exists(Backend b, byte[]... keys) {
        return Decode.intVal(b.exec(Argv.cmd("EXISTS").addAll(keys).list()));
    }

    static long incr(Backend b, byte[] key) {
        return Decode.intVal(b.exec(Argv.cmd("INCR").add(key).list()));
    }

    static long incrBy(Backend b, byte[] key, long delta) {
        return Decode.intVal(b.exec(Argv.cmd("INCRBY").add(key).addLong(delta).list()));
    }

    static boolean expire(Backend b, byte[] key, Duration ttl) {
        return Decode.bool(b.exec(Argv.cmd("PEXPIRE").add(key).addLong(Bytes.toMs(ttl)).list()));
    }

    static boolean persist(Backend b, byte[] key) {
        return Decode.bool(b.exec(Argv.cmd("PERSIST").add(key).list()));
    }

    static long ttlMs(Backend b, byte[] key) {
        return Decode.intVal(b.exec(Argv.cmd("PTTL").add(key).list()));
    }

    static String typeOf(Backend b, byte[] key) {
        return Decode.str(b.exec(Argv.cmd("TYPE").add(key).list()));
    }

    static long dbsize(Backend b) {
        return Decode.intVal(b.exec(Argv.cmd("DBSIZE").list()));
    }

    static void flushall(Backend b) {
        Decode.ok(b.exec(Argv.cmd("FLUSHALL").list()));
    }

    static void setWithTtl(Backend b, byte[] key, byte[] value, Duration ttl) {
        b.fastSet(key, value, Math.max(1, Bytes.toMs(ttl)));
    }

    static List<byte[]> mget(Backend b, byte[]... keys) {
        return Decode.optBulks(b.exec(Argv.cmd("MGET").addAll(keys).list()));
    }

    static void mset(Backend b, List<byte[]> pairsFlat) {
        Decode.ok(b.exec(Argv.cmd("MSET").addAll(pairsFlat).list()));
    }

    static long publish(Backend b, byte[] channel, byte[] message) {
        return Decode.intVal(b.exec(Argv.cmd("PUBLISH").add(channel).add(message).list()));
    }
}

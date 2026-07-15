// KevyAsyncClient — the asynchronous face of one KevyClient (client-contract
// §1.4). Every command returns a CompletableFuture; each runs the very same
// blocking operation on the shared connection (serialized by SyncBackend), so
// the two faces provably agree on results. Java has blocking sockets, so this
// works for BOTH backends (embedded and remote), unlike the JS/TS exception.
//
// Work is scheduled on a single daemon thread: futures for one client execute
// in submission order, and the SyncBackend lock keeps a concurrently-used
// blocking face safe. A KevyException raised by the operation completes the
// future exceptionally.
package jp.golia.kevy;

import java.time.Duration;
import java.util.List;
import java.util.Optional;
import java.util.OptionalDouble;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.function.Supplier;

public final class KevyAsyncClient {
    private final KevyClient c;
    private final ExecutorService pool;

    KevyAsyncClient(KevyClient client) {
        this.c = client;
        this.pool = Executors.newSingleThreadExecutor(r -> {
            Thread t = new Thread(r, "kevy-async");
            t.setDaemon(true);
            return t;
        });
    }

    /** The blocking twin — same connection, same store. */
    public KevyClient blocking() {
        return c;
    }

    private <T> CompletableFuture<T> run(Supplier<T> op) {
        return CompletableFuture.supplyAsync(op, pool);
    }

    // ---- raw + core (§3.1) ----
    public CompletableFuture<Reply> execute(byte[]... argv) { return run(() -> c.execute(argv)); }
    public CompletableFuture<Void> ping() { return run(() -> { c.ping(); return null; }); }
    public CompletableFuture<Void> set(byte[] k, byte[] v) { return run(() -> { c.set(k, v); return null; }); }
    public CompletableFuture<Void> set(String k, String v) { return run(() -> { c.set(k, v); return null; }); }
    public CompletableFuture<Optional<byte[]>> get(byte[] k) { return run(() -> c.get(k)); }
    public CompletableFuture<Optional<byte[]>> get(String k) { return run(() -> c.get(k)); }
    public CompletableFuture<Long> del(byte[]... keys) { return run(() -> c.del(keys)); }
    public CompletableFuture<Long> del(String... keys) { return run(() -> c.del(keys)); }
    public CompletableFuture<Long> exists(byte[]... keys) { return run(() -> c.exists(keys)); }
    public CompletableFuture<Long> incr(byte[] k) { return run(() -> c.incr(k)); }
    public CompletableFuture<Long> incr(String k) { return run(() -> c.incr(k)); }
    public CompletableFuture<Long> incrBy(byte[] k, long d) { return run(() -> c.incrBy(k, d)); }
    public CompletableFuture<Boolean> expire(byte[] k, Duration ttl) { return run(() -> c.expire(k, ttl)); }
    public CompletableFuture<Boolean> persist(byte[] k) { return run(() -> c.persist(k)); }
    public CompletableFuture<Long> ttlMs(byte[] k) { return run(() -> c.ttlMs(k)); }
    public CompletableFuture<String> typeOf(byte[] k) { return run(() -> c.typeOf(k)); }
    public CompletableFuture<Long> dbsize() { return run(c::dbsize); }
    public CompletableFuture<Void> flushall() { return run(() -> { c.flushall(); return null; }); }
    public CompletableFuture<Void> setWithTtl(byte[] k, byte[] v, Duration ttl) { return run(() -> { c.setWithTtl(k, v, ttl); return null; }); }
    public CompletableFuture<List<byte[]>> mget(byte[]... keys) { return run(() -> c.mget(keys)); }
    public CompletableFuture<Void> mset(byte[]... kv) { return run(() -> { c.mset(kv); return null; }); }
    public CompletableFuture<Long> publish(byte[] ch, byte[] msg) { return run(() -> c.publish(ch, msg)); }
    public CompletableFuture<Long> publish(String ch, String msg) { return run(() -> c.publish(ch, msg)); }

    // ---- hash (§3.2) ----
    public CompletableFuture<Long> hset(byte[] k, byte[]... fv) { return run(() -> c.hset(k, fv)); }
    public CompletableFuture<Optional<byte[]>> hget(byte[] k, byte[] f) { return run(() -> c.hget(k, f)); }
    public CompletableFuture<Long> hdel(byte[] k, byte[]... f) { return run(() -> c.hdel(k, f)); }
    public CompletableFuture<Long> hlen(byte[] k) { return run(() -> c.hlen(k)); }
    public CompletableFuture<List<byte[]>> hgetall(byte[] k) { return run(() -> c.hgetall(k)); }
    public CompletableFuture<List<byte[]>> hkeys(byte[] k) { return run(() -> c.hkeys(k)); }
    public CompletableFuture<List<byte[]>> hvals(byte[] k) { return run(() -> c.hvals(k)); }

    // ---- list (§3.3) ----
    public CompletableFuture<Long> lpush(byte[] k, byte[]... v) { return run(() -> c.lpush(k, v)); }
    public CompletableFuture<Long> rpush(byte[] k, byte[]... v) { return run(() -> c.rpush(k, v)); }
    public CompletableFuture<List<byte[]>> lpop(byte[] k, long n) { return run(() -> c.lpop(k, n)); }
    public CompletableFuture<List<byte[]>> rpop(byte[] k, long n) { return run(() -> c.rpop(k, n)); }
    public CompletableFuture<Long> llen(byte[] k) { return run(() -> c.llen(k)); }
    public CompletableFuture<List<byte[]>> lrange(byte[] k, long a, long b) { return run(() -> c.lrange(k, a, b)); }

    // ---- set (§3.4) ----
    public CompletableFuture<Long> sadd(byte[] k, byte[]... m) { return run(() -> c.sadd(k, m)); }
    public CompletableFuture<Long> srem(byte[] k, byte[]... m) { return run(() -> c.srem(k, m)); }
    public CompletableFuture<List<byte[]>> smembers(byte[] k) { return run(() -> c.smembers(k)); }
    public CompletableFuture<Long> scard(byte[] k) { return run(() -> c.scard(k)); }
    public CompletableFuture<Boolean> sismember(byte[] k, byte[] m) { return run(() -> c.sismember(k, m)); }
    public CompletableFuture<List<byte[]>> sinter(byte[]... keys) { return run(() -> c.sinter(keys)); }
    public CompletableFuture<List<byte[]>> sunion(byte[]... keys) { return run(() -> c.sunion(keys)); }
    public CompletableFuture<List<byte[]>> sdiff(byte[]... keys) { return run(() -> c.sdiff(keys)); }

    // ---- zset (§3.5) + algebra (§3.6) ----
    public CompletableFuture<Long> zadd(byte[] k, double s, byte[] m) { return run(() -> c.zadd(k, s, m)); }
    public CompletableFuture<Long> zrem(byte[] k, byte[]... m) { return run(() -> c.zrem(k, m)); }
    public CompletableFuture<OptionalDouble> zscore(byte[] k, byte[] m) { return run(() -> c.zscore(k, m)); }
    public CompletableFuture<Long> zcard(byte[] k) { return run(() -> c.zcard(k)); }
    public CompletableFuture<List<byte[]>> zrange(byte[] k, long a, long b) { return run(() -> c.zrange(k, a, b)); }
    public CompletableFuture<Long> zinterstore(byte[] dest, byte[]... keys) { return run(() -> c.zinterstore(dest, keys)); }
    public CompletableFuture<Long> zunionstore(byte[] dest, byte[]... keys) { return run(() -> c.zunionstore(dest, keys)); }
    public CompletableFuture<Long> zintercard(byte[][] keys, Long limit) { return run(() -> c.zintercard(keys, limit)); }

    // ---- hash-field TTL (§3.7) ----
    public CompletableFuture<long[]> hexpire(byte[] k, byte[][] f, Duration ttl, HExpireCond cond) { return run(() -> c.hexpire(k, f, ttl, cond)); }
    public CompletableFuture<long[]> hpexpire(byte[] k, byte[][] f, Duration ttl, HExpireCond cond) { return run(() -> c.hpexpire(k, f, ttl, cond)); }
    public CompletableFuture<long[]> httl(byte[] k, byte[][] f) { return run(() -> c.httl(k, f)); }
    public CompletableFuture<long[]> hpttl(byte[] k, byte[][] f) { return run(() -> c.hpttl(k, f)); }
    public CompletableFuture<long[]> hpersist(byte[] k, byte[][] f) { return run(() -> c.hpersist(k, f)); }

    // ---- blocking pops (§3.14) ----
    public CompletableFuture<Optional<KeyValue>> blpop(byte[][] keys, Duration t) { return run(() -> c.blpop(keys, t)); }
    public CompletableFuture<Optional<KeyValue>> brpop(byte[][] keys, Duration t) { return run(() -> c.brpop(keys, t)); }
    public CompletableFuture<Optional<ZPopHit>> bzpopmin(byte[][] keys, Duration t) { return run(() -> c.bzpopmin(keys, t)); }

    // ---- pipeline (§3.13) ----
    public CompletableFuture<List<Reply>> pipeline(java.util.function.Consumer<PipelineBuf> build) { return run(() -> c.pipeline(build)); }

    void shutdown() {
        pool.shutdown();
    }
}

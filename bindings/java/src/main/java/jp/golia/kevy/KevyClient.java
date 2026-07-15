// KevyClient — the unified blocking client (client-contract §1.4, §3). One
// client, chosen by URL: mem:// / file:// run against an in-process embedded
// store; kevy:// / redis:// / tcp:// run against a remote RESP server. Every
// command is here as a typed method; the async face (`async()`) exposes the
// same operations as CompletableFutures over one shared connection.
//
// All keys/values are binary-safe byte[] (the canonical form, §7); String
// overloads are provided for the common surface (UTF-8 encoded). The raw
// escape hatch is `execute(...)` / `cmd(...)`, which return the Reply as data
// (an -ERR frame is NOT thrown there) — reach for it for any verb the typed
// API doesn't wrap.
package jp.golia.kevy;

import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.OptionalDouble;
import java.util.function.Consumer;

public final class KevyClient implements AutoCloseable {
    private final SyncBackend backend;
    private final String url;
    private volatile KevyAsyncClient asyncFace;

    KevyClient(Backend raw, String url) {
        this.backend = new SyncBackend(raw);
        this.url = url;
    }

    /** Open a client from a connect URL (client-contract §1.1). */
    public static KevyClient connect(String url) {
        return Kevy.connect(url);
    }

    /** The async face — same operations, returning CompletableFutures. */
    public KevyAsyncClient async() {
        KevyAsyncClient a = asyncFace;
        if (a == null) {
            synchronized (this) {
                if (asyncFace == null) asyncFace = new KevyAsyncClient(this);
                a = asyncFace;
            }
        }
        return a;
    }

    public boolean isEmbedded() {
        return backend.embedded();
    }

    Backend backend() {
        return backend;
    }

    SyncBackend syncBackend() {
        return backend;
    }

    // ---- raw escape hatch (§7) — returns the reply as data, never throws on -ERR ----
    public Reply execute(byte[]... argv) {
        return backend.exec(new ArrayList<>(List.of(argv)));
    }

    public Reply execute(String... argv) {
        List<byte[]> a = new ArrayList<>(argv.length);
        for (String s : argv) a.add(Bytes.of(s));
        return backend.exec(a);
    }

    public Reply cmd(List<byte[]> argv) {
        return backend.exec(new ArrayList<>(argv));
    }

    // ---- Core string / generic (§3.1) ----
    public void ping() { CoreOps.ping(backend); }
    public void set(byte[] key, byte[] value) { CoreOps.set(backend, key, value); }
    public void set(String key, String value) { set(Bytes.of(key), Bytes.of(value)); }
    public Optional<byte[]> get(byte[] key) { return CoreOps.get(backend, key); }
    public Optional<byte[]> get(String key) { return get(Bytes.of(key)); }
    public long del(byte[]... keys) { return CoreOps.del(backend, keys); }
    public long del(String... keys) { return CoreOps.del(backend, encode(keys)); }
    public long exists(byte[]... keys) { return CoreOps.exists(backend, keys); }
    public long exists(String... keys) { return CoreOps.exists(backend, encode(keys)); }
    public long incr(byte[] key) { return CoreOps.incr(backend, key); }
    public long incr(String key) { return incr(Bytes.of(key)); }
    public long incrBy(byte[] key, long delta) { return CoreOps.incrBy(backend, key, delta); }
    public long incrBy(String key, long delta) { return incrBy(Bytes.of(key), delta); }
    public boolean expire(byte[] key, Duration ttl) { return CoreOps.expire(backend, key, ttl); }
    public boolean expire(String key, Duration ttl) { return expire(Bytes.of(key), ttl); }
    public boolean persist(byte[] key) { return CoreOps.persist(backend, key); }
    public boolean persist(String key) { return persist(Bytes.of(key)); }
    public long ttlMs(byte[] key) { return CoreOps.ttlMs(backend, key); }
    public long ttlMs(String key) { return ttlMs(Bytes.of(key)); }
    public String typeOf(byte[] key) { return CoreOps.typeOf(backend, key); }
    public String typeOf(String key) { return typeOf(Bytes.of(key)); }
    public long dbsize() { return CoreOps.dbsize(backend); }
    public void flushall() { CoreOps.flushall(backend); }
    public void setWithTtl(byte[] key, byte[] value, Duration ttl) { CoreOps.setWithTtl(backend, key, value, ttl); }
    public void setWithTtl(String key, String value, Duration ttl) { setWithTtl(Bytes.of(key), Bytes.of(value), ttl); }
    public List<byte[]> mget(byte[]... keys) { return CoreOps.mget(backend, keys); }
    public List<byte[]> mget(String... keys) { return CoreOps.mget(backend, encode(keys)); }
    public void mset(byte[]... keyValueFlat) { CoreOps.mset(backend, List.of(keyValueFlat)); }
    public void mset(String... keyValueFlat) { CoreOps.mset(backend, List.of(encode(keyValueFlat))); }
    public long publish(byte[] channel, byte[] message) { return CoreOps.publish(backend, channel, message); }
    public long publish(String channel, String message) { return publish(Bytes.of(channel), Bytes.of(message)); }

    // ---- Hash (§3.2) ----
    public long hset(byte[] key, byte[]... fieldValueFlat) { return CollOps.hset(backend, key, List.of(fieldValueFlat)); }
    public long hset(String key, String... fieldValueFlat) { return CollOps.hset(backend, Bytes.of(key), List.of(encode(fieldValueFlat))); }
    public Optional<byte[]> hget(byte[] key, byte[] field) { return CollOps.hget(backend, key, field); }
    public Optional<byte[]> hget(String key, String field) { return hget(Bytes.of(key), Bytes.of(field)); }
    public long hdel(byte[] key, byte[]... fields) { return CollOps.hdel(backend, key, fields); }
    public long hdel(String key, String... fields) { return CollOps.hdel(backend, Bytes.of(key), encode(fields)); }
    public long hlen(byte[] key) { return CollOps.hlen(backend, key); }
    public long hlen(String key) { return hlen(Bytes.of(key)); }
    public List<byte[]> hgetall(byte[] key) { return CollOps.hgetall(backend, key); }
    public List<byte[]> hgetall(String key) { return hgetall(Bytes.of(key)); }
    public List<byte[]> hkeys(byte[] key) { return CollOps.hkeys(backend, key); }
    public List<byte[]> hvals(byte[] key) { return CollOps.hvals(backend, key); }

    // ---- List (§3.3) ----
    public long lpush(byte[] key, byte[]... values) { return CollOps.lpush(backend, key, values); }
    public long lpush(String key, String... values) { return CollOps.lpush(backend, Bytes.of(key), encode(values)); }
    public long rpush(byte[] key, byte[]... values) { return CollOps.rpush(backend, key, values); }
    public long rpush(String key, String... values) { return CollOps.rpush(backend, Bytes.of(key), encode(values)); }
    public List<byte[]> lpop(byte[] key, long count) { return CollOps.lpop(backend, key, count); }
    public List<byte[]> lpop(String key, long count) { return lpop(Bytes.of(key), count); }
    public List<byte[]> rpop(byte[] key, long count) { return CollOps.rpop(backend, key, count); }
    public List<byte[]> rpop(String key, long count) { return rpop(Bytes.of(key), count); }
    public long llen(byte[] key) { return CollOps.llen(backend, key); }
    public long llen(String key) { return llen(Bytes.of(key)); }
    public List<byte[]> lrange(byte[] key, long start, long stop) { return CollOps.lrange(backend, key, start, stop); }
    public List<byte[]> lrange(String key, long start, long stop) { return lrange(Bytes.of(key), start, stop); }

    // ---- Set (§3.4) ----
    public long sadd(byte[] key, byte[]... members) { return CollOps.sadd(backend, key, members); }
    public long sadd(String key, String... members) { return CollOps.sadd(backend, Bytes.of(key), encode(members)); }
    public long srem(byte[] key, byte[]... members) { return CollOps.srem(backend, key, members); }
    public long srem(String key, String... members) { return CollOps.srem(backend, Bytes.of(key), encode(members)); }
    public List<byte[]> smembers(byte[] key) { return CollOps.smembers(backend, key); }
    public List<byte[]> smembers(String key) { return smembers(Bytes.of(key)); }
    public long scard(byte[] key) { return CollOps.scard(backend, key); }
    public boolean sismember(byte[] key, byte[] member) { return CollOps.sismember(backend, key, member); }
    public boolean sismember(String key, String member) { return sismember(Bytes.of(key), Bytes.of(member)); }
    public List<byte[]> sinter(byte[]... keys) { return CollOps.sinter(backend, keys); }
    public List<byte[]> sinter(String... keys) { return CollOps.sinter(backend, encode(keys)); }
    public List<byte[]> sunion(byte[]... keys) { return CollOps.sunion(backend, keys); }
    public List<byte[]> sunion(String... keys) { return CollOps.sunion(backend, encode(keys)); }
    public List<byte[]> sdiff(byte[]... keys) { return CollOps.sdiff(backend, keys); }
    public List<byte[]> sdiff(String... keys) { return CollOps.sdiff(backend, encode(keys)); }

    // ---- Sorted set (§3.5) ----
    public long zadd(byte[] key, List<ZMember> members) { return CollOps.zadd(backend, key, members); }
    public long zadd(byte[] key, double score, byte[] member) { return CollOps.zadd(backend, key, List.of(new ZMember(score, member))); }
    public long zadd(String key, double score, String member) { return zadd(Bytes.of(key), score, Bytes.of(member)); }
    public long zrem(byte[] key, byte[]... members) { return CollOps.zrem(backend, key, members); }
    public long zrem(String key, String... members) { return CollOps.zrem(backend, Bytes.of(key), encode(members)); }
    public OptionalDouble zscore(byte[] key, byte[] member) { return CollOps.zscore(backend, key, member); }
    public OptionalDouble zscore(String key, String member) { return zscore(Bytes.of(key), Bytes.of(member)); }
    public long zcard(byte[] key) { return CollOps.zcard(backend, key); }
    public long zcard(String key) { return zcard(Bytes.of(key)); }
    public List<byte[]> zrange(byte[] key, long start, long stop) { return CollOps.zrange(backend, key, start, stop); }
    public List<byte[]> zrange(String key, long start, long stop) { return zrange(Bytes.of(key), start, stop); }

    // ---- Sorted-set algebra (§3.6) ----
    public long zinterstore(byte[] dest, byte[]... keys) { return ZAlgebraOps.zinterstore(backend, dest, keys); }
    public long zunionstore(byte[] dest, byte[]... keys) { return ZAlgebraOps.zunionstore(backend, dest, keys); }
    public long zinterstoreWith(byte[] dest, byte[][] keys, double[] weights, ZAggregate agg) { return ZAlgebraOps.zinterstoreWith(backend, dest, keys, weights, agg); }
    public long zunionstoreWith(byte[] dest, byte[][] keys, double[] weights, ZAggregate agg) { return ZAlgebraOps.zunionstoreWith(backend, dest, keys, weights, agg); }
    public long zintercard(byte[][] keys, Long limit) { return ZAlgebraOps.zintercard(backend, keys, limit); }

    // ---- Hash-field TTL (§3.7) ----
    public long[] hexpire(byte[] key, byte[][] fields, Duration ttl, HExpireCond cond) { return HashTtlOps.hexpire(backend, key, fields, ttl, cond); }
    public long[] hpexpire(byte[] key, byte[][] fields, Duration ttl, HExpireCond cond) { return HashTtlOps.hpexpire(backend, key, fields, ttl, cond); }
    public long[] hpersist(byte[] key, byte[][] fields) { return HashTtlOps.hpersist(backend, key, fields); }
    public long[] httl(byte[] key, byte[][] fields) { return HashTtlOps.httl(backend, key, fields); }
    public long[] hpttl(byte[] key, byte[][] fields) { return HashTtlOps.hpttl(backend, key, fields); }

    // ---- Blocking pops (§3.14) ----
    public Optional<KeyValue> blpop(byte[][] keys, Duration timeout) { return BlockingOps.blpop(backend, keys, timeout); }
    public Optional<KeyValue> brpop(byte[][] keys, Duration timeout) { return BlockingOps.brpop(backend, keys, timeout); }
    public Optional<ZPopHit> bzpopmin(byte[][] keys, Duration timeout) { return BlockingOps.bzpopmin(backend, keys, timeout); }

    // ---- IDX.* (§3.8) — remote-only ----
    public void idxCreateRange(String name, String prefix, String field, IdxType ty) { IdxOps.createRange(backend, name, prefix, field, ty); }
    public void idxCreateRaw(List<byte[]> args) { IdxOps.createRaw(backend, args); }
    public boolean idxDrop(String name) { return IdxOps.drop(backend, name); }
    public List<IdxInfo> idxList() { return IdxOps.list(backend); }
    public IdxPage idxQueryRange(String name, byte[] min, byte[] max, long limit, byte[] cursor) { return IdxOps.queryRange(backend, name, min, max, limit, cursor); }
    public IdxPage idxQueryEq(String name, byte[] value, long limit) { return IdxOps.queryEq(backend, name, value, limit); }
    public List<Scored> idxQueryMatch(String name, String text, long limit) { return IdxOps.queryMatch(backend, name, text, limit); }
    public List<Scored> idxQueryKnn(String name, float[] vector, long k) { return IdxOps.queryKnn(backend, name, vector, k); }
    public Reply idxQueryRaw(List<byte[]> args) { return IdxOps.queryRaw(backend, args); }

    // ---- FEED.* (§3.10) ----
    public long feedShards() { return FeedOps.shards(backend); }
    public FeedTail feedTail(long shard) { return FeedOps.tail(backend, shard); }
    public FeedBatch feedRead(long shard, long generation, long offset, Long count, byte[]... prefixes) { return FeedOps.read(backend, shard, generation, offset, count, prefixes); }

    // ---- Transactions (§3.12) — remote-only ----
    public void watch(byte[]... keys) { Transaction.watch(this, keys); }
    public void unwatch() { Transaction.unwatch(this); }
    public Transaction multi() { return Transaction.begin(this); }

    // ---- Pipeline (§3.13) — remote-only ----
    public List<Reply> pipeline(Consumer<PipelineBuf> build) { return PipelineBuf.run(this, build); }

    public String url() { return url; }

    @Override
    public void close() {
        if (asyncFace != null) asyncFace.shutdown();
        backend.close();
    }

    private static byte[][] encode(String[] ss) {
        byte[][] out = new byte[ss.length][];
        for (int i = 0; i < ss.length; i++) out[i] = Bytes.of(ss[i]);
        return out;
    }
}

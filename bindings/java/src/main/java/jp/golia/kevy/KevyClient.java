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

    /** True for a mem:// / file:// (embedded) client, false for a remote one. */
    public boolean isEmbedded() {
        return backend.embedded();
    }

    Backend backend() {
        return backend;
    }

    SyncBackend syncBackend() {
        return backend;
    }

    // ---- raw escape hatch (§7) ----

    /**
     * Run any verb by raw argv and get the reply back as data. Unlike the typed
     * methods, an {@code -ERR} frame is NOT thrown here — it surfaces as a
     * {@link Reply.Error}. Reach for this for verbs the typed API doesn't wrap.
     * @param argv command name then arguments, each binary-safe
     * @return the decoded reply (including error frames)
     */
    public Reply execute(byte[]... argv) {
        return backend.exec(new ArrayList<>(List.of(argv)));
    }

    /** {@link #execute(byte[][])} taking UTF-8 String arguments. */
    public Reply execute(String... argv) {
        List<byte[]> a = new ArrayList<>(argv.length);
        for (String s : argv) a.add(Bytes.of(s));
        return backend.exec(a);
    }

    /** {@link #execute(byte[][])} taking argv as a List. */
    public Reply cmd(List<byte[]> argv) {
        return backend.exec(new ArrayList<>(argv));
    }

    // ---- Core string / generic (§3.1). String overloads are UTF-8 twins. ----

    /** PING the server; throws on a non-PONG reply. */
    public void ping() { CoreOps.ping(backend); }
    /** SET key to value (no expiry). */
    public void set(byte[] key, byte[] value) { CoreOps.set(backend, key, value); }
    public void set(String key, String value) { set(Bytes.of(key), Bytes.of(value)); }
    /** GET a key's raw value bytes (NOT a String); empty Optional on a miss. */
    public Optional<byte[]> get(byte[] key) { return CoreOps.get(backend, key); }
    public Optional<byte[]> get(String key) { return get(Bytes.of(key)); }
    /** DEL the given keys; returns how many existed. */
    public long del(byte[]... keys) { return CoreOps.del(backend, keys); }
    public long del(String... keys) { return CoreOps.del(backend, encode(keys)); }
    /** EXISTS count across the given keys (duplicates counted). */
    public long exists(byte[]... keys) { return CoreOps.exists(backend, keys); }
    public long exists(String... keys) { return CoreOps.exists(backend, encode(keys)); }
    /** INCR key by 1; returns the new value. Throws if the value isn't an int. */
    public long incr(byte[] key) { return CoreOps.incr(backend, key); }
    public long incr(String key) { return incr(Bytes.of(key)); }
    /** INCRBY key by delta (may be negative); returns the new value. */
    public long incrBy(byte[] key, long delta) { return CoreOps.incrBy(backend, key, delta); }
    public long incrBy(String key, long delta) { return incrBy(Bytes.of(key), delta); }
    /** EXPIRE key after ttl; false if the key is absent. */
    public boolean expire(byte[] key, Duration ttl) { return CoreOps.expire(backend, key, ttl); }
    public boolean expire(String key, Duration ttl) { return expire(Bytes.of(key), ttl); }
    /** PERSIST key (clear its TTL); false if absent or already persistent. */
    public boolean persist(byte[] key) { return CoreOps.persist(backend, key); }
    public boolean persist(String key) { return persist(Bytes.of(key)); }
    /** Remaining TTL in ms; -1 = no expiry, -2 = key absent. */
    public long ttlMs(byte[] key) { return CoreOps.ttlMs(backend, key); }
    public long ttlMs(String key) { return ttlMs(Bytes.of(key)); }
    /** TYPE of the key ("string"/"list"/… ; "none" if absent). */
    public String typeOf(byte[] key) { return CoreOps.typeOf(backend, key); }
    public String typeOf(String key) { return typeOf(Bytes.of(key)); }
    /** DBSIZE — number of keys in the current db. */
    public long dbsize() { return CoreOps.dbsize(backend); }
    /** FLUSHALL — remove every key. */
    public void flushall() { CoreOps.flushall(backend); }
    /** SET key to value with an expiry. */
    public void setWithTtl(byte[] key, byte[] value, Duration ttl) { CoreOps.setWithTtl(backend, key, value, ttl); }
    public void setWithTtl(String key, String value, Duration ttl) { setWithTtl(Bytes.of(key), Bytes.of(value), ttl); }
    /** MGET the keys; each element is the value bytes or null on a miss. */
    public List<byte[]> mget(byte[]... keys) { return CoreOps.mget(backend, keys); }
    public List<byte[]> mget(String... keys) { return CoreOps.mget(backend, encode(keys)); }
    /** MSET flat key,value,key,value… pairs. */
    public void mset(byte[]... keyValueFlat) { CoreOps.mset(backend, List.of(keyValueFlat)); }
    public void mset(String... keyValueFlat) { CoreOps.mset(backend, List.of(encode(keyValueFlat))); }
    /** PUBLISH message to channel; returns the number of receivers. */
    public long publish(byte[] channel, byte[] message) { return CoreOps.publish(backend, channel, message); }
    public long publish(String channel, String message) { return publish(Bytes.of(channel), Bytes.of(message)); }

    // ---- Hash (§3.2). String overloads are UTF-8 twins. ----

    /** HSET flat field,value pairs; returns how many fields were newly added. */
    public long hset(byte[] key, byte[]... fieldValueFlat) { return CollOps.hset(backend, key, List.of(fieldValueFlat)); }
    public long hset(String key, String... fieldValueFlat) { return CollOps.hset(backend, Bytes.of(key), List.of(encode(fieldValueFlat))); }
    /** HGET one field's value bytes; empty Optional if field/key absent. */
    public Optional<byte[]> hget(byte[] key, byte[] field) { return CollOps.hget(backend, key, field); }
    public Optional<byte[]> hget(String key, String field) { return hget(Bytes.of(key), Bytes.of(field)); }
    /** HDEL fields; returns how many were removed. */
    public long hdel(byte[] key, byte[]... fields) { return CollOps.hdel(backend, key, fields); }
    public long hdel(String key, String... fields) { return CollOps.hdel(backend, Bytes.of(key), encode(fields)); }
    /** HLEN — number of fields in the hash. */
    public long hlen(byte[] key) { return CollOps.hlen(backend, key); }
    public long hlen(String key) { return hlen(Bytes.of(key)); }
    /** HGETALL as a flat field,value,… list. */
    public List<byte[]> hgetall(byte[] key) { return CollOps.hgetall(backend, key); }
    public List<byte[]> hgetall(String key) { return hgetall(Bytes.of(key)); }
    /** HKEYS — every field name. */
    public List<byte[]> hkeys(byte[] key) { return CollOps.hkeys(backend, key); }
    public List<byte[]> hkeys(String key) { return hkeys(Bytes.of(key)); }
    /** HVALS — every field value. */
    public List<byte[]> hvals(byte[] key) { return CollOps.hvals(backend, key); }
    public List<byte[]> hvals(String key) { return hvals(Bytes.of(key)); }

    // ---- List (§3.3). String overloads are UTF-8 twins. ----

    /** LPUSH values (left); returns the new list length. */
    public long lpush(byte[] key, byte[]... values) { return CollOps.lpush(backend, key, values); }
    public long lpush(String key, String... values) { return CollOps.lpush(backend, Bytes.of(key), encode(values)); }
    /** RPUSH values (right); returns the new list length. */
    public long rpush(byte[] key, byte[]... values) { return CollOps.rpush(backend, key, values); }
    public long rpush(String key, String... values) { return CollOps.rpush(backend, Bytes.of(key), encode(values)); }
    /** LPOP up to count elements off the left; empty list if none. */
    public List<byte[]> lpop(byte[] key, long count) { return CollOps.lpop(backend, key, count); }
    public List<byte[]> lpop(String key, long count) { return lpop(Bytes.of(key), count); }
    /** RPOP up to count elements off the right; empty list if none. */
    public List<byte[]> rpop(byte[] key, long count) { return CollOps.rpop(backend, key, count); }
    public List<byte[]> rpop(String key, long count) { return rpop(Bytes.of(key), count); }
    /** LLEN — list length. */
    public long llen(byte[] key) { return CollOps.llen(backend, key); }
    public long llen(String key) { return llen(Bytes.of(key)); }
    /** LRANGE [start,stop] inclusive; negative indices count from the tail. */
    public List<byte[]> lrange(byte[] key, long start, long stop) { return CollOps.lrange(backend, key, start, stop); }
    public List<byte[]> lrange(String key, long start, long stop) { return lrange(Bytes.of(key), start, stop); }

    // ---- Set (§3.4). String overloads are UTF-8 twins. ----

    /** SADD members; returns how many were newly added. */
    public long sadd(byte[] key, byte[]... members) { return CollOps.sadd(backend, key, members); }
    public long sadd(String key, String... members) { return CollOps.sadd(backend, Bytes.of(key), encode(members)); }
    /** SREM members; returns how many were removed. */
    public long srem(byte[] key, byte[]... members) { return CollOps.srem(backend, key, members); }
    public long srem(String key, String... members) { return CollOps.srem(backend, Bytes.of(key), encode(members)); }
    /** SMEMBERS — all members (unordered). */
    public List<byte[]> smembers(byte[] key) { return CollOps.smembers(backend, key); }
    public List<byte[]> smembers(String key) { return smembers(Bytes.of(key)); }
    /** SCARD — set cardinality. */
    public long scard(byte[] key) { return CollOps.scard(backend, key); }
    public long scard(String key) { return scard(Bytes.of(key)); }
    /** SISMEMBER — whether member is in the set. */
    public boolean sismember(byte[] key, byte[] member) { return CollOps.sismember(backend, key, member); }
    public boolean sismember(String key, String member) { return sismember(Bytes.of(key), Bytes.of(member)); }
    /** SINTER — intersection of the given sets. */
    public List<byte[]> sinter(byte[]... keys) { return CollOps.sinter(backend, keys); }
    public List<byte[]> sinter(String... keys) { return CollOps.sinter(backend, encode(keys)); }
    /** SUNION — union of the given sets. */
    public List<byte[]> sunion(byte[]... keys) { return CollOps.sunion(backend, keys); }
    public List<byte[]> sunion(String... keys) { return CollOps.sunion(backend, encode(keys)); }
    /** SDIFF — first set minus the rest. */
    public List<byte[]> sdiff(byte[]... keys) { return CollOps.sdiff(backend, keys); }
    public List<byte[]> sdiff(String... keys) { return CollOps.sdiff(backend, encode(keys)); }

    // ---- Sorted set (§3.5). String overloads are UTF-8 twins. ----

    /** ZADD scored members; returns how many were newly added. */
    public long zadd(byte[] key, List<ZMember> members) { return CollOps.zadd(backend, key, members); }
    /** ZADD one score/member. */
    public long zadd(byte[] key, double score, byte[] member) { return CollOps.zadd(backend, key, List.of(new ZMember(score, member))); }
    public long zadd(String key, double score, String member) { return zadd(Bytes.of(key), score, Bytes.of(member)); }
    /** ZREM members; returns how many were removed. */
    public long zrem(byte[] key, byte[]... members) { return CollOps.zrem(backend, key, members); }
    public long zrem(String key, String... members) { return CollOps.zrem(backend, Bytes.of(key), encode(members)); }
    /** ZSCORE — member's score; empty OptionalDouble if absent. */
    public OptionalDouble zscore(byte[] key, byte[] member) { return CollOps.zscore(backend, key, member); }
    public OptionalDouble zscore(String key, String member) { return zscore(Bytes.of(key), Bytes.of(member)); }
    /** ZCARD — number of members. */
    public long zcard(byte[] key) { return CollOps.zcard(backend, key); }
    public long zcard(String key) { return zcard(Bytes.of(key)); }
    /** ZRANGE [start,stop] by rank, ascending score. */
    public List<byte[]> zrange(byte[] key, long start, long stop) { return CollOps.zrange(backend, key, start, stop); }
    public List<byte[]> zrange(String key, long start, long stop) { return zrange(Bytes.of(key), start, stop); }

    // ---- Sorted-set algebra (§3.6) ----

    /** ZINTERSTORE the intersection into dest; returns the result cardinality. */
    public long zinterstore(byte[] dest, byte[]... keys) { return ZAlgebraOps.zinterstore(backend, dest, keys); }
    /** ZUNIONSTORE the union into dest; returns the result cardinality. */
    public long zunionstore(byte[] dest, byte[]... keys) { return ZAlgebraOps.zunionstore(backend, dest, keys); }
    /** ZINTERSTORE with per-key WEIGHTS and an AGGREGATE mode. */
    public long zinterstoreWith(byte[] dest, byte[][] keys, double[] weights, ZAggregate agg) { return ZAlgebraOps.zinterstoreWith(backend, dest, keys, weights, agg); }
    /** ZUNIONSTORE with per-key WEIGHTS and an AGGREGATE mode. */
    public long zunionstoreWith(byte[] dest, byte[][] keys, double[] weights, ZAggregate agg) { return ZAlgebraOps.zunionstoreWith(backend, dest, keys, weights, agg); }
    /** ZINTERCARD — intersection size, optionally LIMITed (null = no limit). */
    public long zintercard(byte[][] keys, Long limit) { return ZAlgebraOps.zintercard(backend, keys, limit); }

    // ---- Hash-field TTL (§3.7). Per-field result codes, one per field. ----

    /** HEXPIRE fields after ttl under cond; one status code per field. */
    public long[] hexpire(byte[] key, byte[][] fields, Duration ttl, HExpireCond cond) { return HashTtlOps.hexpire(backend, key, fields, ttl, cond); }
    /** HPEXPIRE (ms precision) fields after ttl under cond. */
    public long[] hpexpire(byte[] key, byte[][] fields, Duration ttl, HExpireCond cond) { return HashTtlOps.hpexpire(backend, key, fields, ttl, cond); }
    /** HPERSIST — clear each field's TTL; one status code per field. */
    public long[] hpersist(byte[] key, byte[][] fields) { return HashTtlOps.hpersist(backend, key, fields); }
    /** HTTL — remaining TTL (seconds) per field. */
    public long[] httl(byte[] key, byte[][] fields) { return HashTtlOps.httl(backend, key, fields); }
    /** HPTTL — remaining TTL (ms) per field. */
    public long[] hpttl(byte[] key, byte[][] fields) { return HashTtlOps.hpttl(backend, key, fields); }

    // ---- Blocking pops (§3.14). Empty Optional on timeout. ----

    /** BLPOP — block up to timeout for a left element across keys. */
    public Optional<KeyValue> blpop(byte[][] keys, Duration timeout) { return BlockingOps.blpop(backend, keys, timeout); }
    /** BRPOP — block up to timeout for a right element across keys. */
    public Optional<KeyValue> brpop(byte[][] keys, Duration timeout) { return BlockingOps.brpop(backend, keys, timeout); }
    /** BZPOPMIN — block up to timeout for the lowest-scored member across keys. */
    public Optional<ZPopHit> bzpopmin(byte[][] keys, Duration timeout) { return BlockingOps.bzpopmin(backend, keys, timeout); }

    // ---- IDX.* (§3.8) — remote-only; embedded throws UnsupportedException. ----

    /** IDX.CREATE a range index over the prefix's given field. */
    public void idxCreateRange(String name, String prefix, String field, IdxType ty) { IdxOps.createRange(backend, name, prefix, field, ty); }
    /** IDX.CREATE from raw trailing args (COMPOSE/HYBRID/GROUPS). */
    public void idxCreateRaw(List<byte[]> args) { IdxOps.createRaw(backend, args); }
    /** IDX.DROP the named index; false if it didn't exist. */
    public boolean idxDrop(String name) { return IdxOps.drop(backend, name); }
    /** IDX.LIST — metadata for every index. */
    public List<IdxInfo> idxList() { return IdxOps.list(backend); }
    /** IDX.QUERY RANGE [min,max] LIMIT; cursor (null = first page) paginates. */
    public IdxPage idxQueryRange(String name, byte[] min, byte[] max, long limit, byte[] cursor) { return IdxOps.queryRange(backend, name, min, max, limit, cursor); }
    /** IDX.QUERY EQ value LIMIT. */
    public IdxPage idxQueryEq(String name, byte[] value, long limit) { return IdxOps.queryEq(backend, name, value, limit); }
    /** IDX.QUERY MATCH full-text; ranked hits with scores. */
    public List<Scored> idxQueryMatch(String name, String text, long limit) { return IdxOps.queryMatch(backend, name, text, limit); }
    /** IDX.QUERY KNN vector search; the k nearest, scored. */
    public List<Scored> idxQueryKnn(String name, float[] vector, long k) { return IdxOps.queryKnn(backend, name, vector, k); }
    /** IDX.QUERY with raw trailing args; the reply as data. */
    public Reply idxQueryRaw(List<byte[]> args) { return IdxOps.queryRaw(backend, args); }

    // ---- FEED.* (§3.10) ----

    /** FEED.SHARDS — number of feed shards. */
    public long feedShards() { return FeedOps.shards(backend); }
    /** FEED.TAIL — the current generation/offset watermark of a shard. */
    public FeedTail feedTail(long shard) { return FeedOps.tail(backend, shard); }
    /** FEED.READ from (generation, offset), up to count, filtered by prefixes. */
    public FeedBatch feedRead(long shard, long generation, long offset, Long count, byte[]... prefixes) { return FeedOps.read(backend, shard, generation, offset, count, prefixes); }

    // ---- Transactions (§3.12) — remote-only ----

    /** WATCH keys for the next MULTI/EXEC (optimistic-locking guard). */
    public void watch(byte[]... keys) { Transaction.watch(this, keys); }
    /** UNWATCH — drop all watches. */
    public void unwatch() { Transaction.unwatch(this); }
    /** MULTI — begin a transaction; queue commands on the returned handle. */
    public Transaction multi() { return Transaction.begin(this); }

    // ---- Pipeline (§3.13) — remote-only ----

    /** Batch commands built by `build`, sent as one write; replies in order. */
    public List<Reply> pipeline(Consumer<PipelineBuf> build) { return PipelineBuf.run(this, build); }

    /** The connect URL this client was opened from. */
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

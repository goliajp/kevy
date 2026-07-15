// ClusterClient — cluster-aware client, remote-only (client-contract §3.15).
// One connection per shard node, CRC16-slot routed so single-key commands hit
// their owner shard directly (no server -MOVED / forwarding hop). Topology is
// discovered once at connect via CLUSTER SLOTS (16384 slots); CRC16 matches
// Redis's key_hash_slot so routing agrees with the server. Requires the server
// in cluster mode.
package jp.golia.kevy;

import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.OptionalDouble;

public final class ClusterClient implements AutoCloseable {
    private static final int NUM_SLOTS = 16384;

    private final List<RespConn> shards;
    private final int[] slotToShard;

    private ClusterClient(List<RespConn> shards, int[] slotToShard) {
        this.shards = shards;
        this.slotToShard = slotToShard;
    }

    private record SlotRange(int start, int end, String host, int port) {}
    private record Node(String host, int port) {}

    /** Connect via a seed node, discover topology, open one conn per shard. */
    public static ClusterClient connect(String host, int port) {
        RespConn seed = RespConn.dial(host, port);
        List<SlotRange> ranges;
        try {
            ranges = parseSlots(seed.request(Argv.cmd("CLUSTER").add("SLOTS").list()));
        } finally {
            seed.close();
        }
        List<Node> nodes = new ArrayList<>();
        int[] table = new int[NUM_SLOTS];
        buildTopology(ranges, nodes, table);
        List<RespConn> conns = new ArrayList<>(nodes.size());
        try {
            for (Node n : nodes) conns.add(RespConn.dial(n.host(), n.port()));
        } catch (RuntimeException e) {
            for (RespConn c : conns) c.close();
            throw e;
        }
        return new ClusterClient(conns, table);
    }

    public int shardCount() {
        return shards.size();
    }

    private RespConn shardFor(byte[] key) {
        return shards.get(slotToShard[Crc16.keyHashSlot(key)]);
    }

    public Reply requestKeyed(byte[] key, byte[]... argv) {
        return shardFor(key).request(new ArrayList<>(List.of(argv)));
    }

    public Reply requestUnkeyed(byte[]... argv) {
        return shards.get(0).request(new ArrayList<>(List.of(argv)));
    }

    public void ping() {
        Reply r = requestUnkeyed(Bytes.of("PING"));
        if (r instanceof Reply.Simple s) {
            String v = Bytes.str(s.value());
            if ("PONG".equals(v) || "OK".equals(v)) return;
        }
        throw r.isError() ? Errors.fromReplyText(r.payload()) : Errors.unexpected(r, "+PONG/+OK");
    }

    public long publish(byte[] channel, byte[] message) {
        return Decode.intVal(requestUnkeyed(Bytes.of("PUBLISH"), channel, message));
    }

    // ---- key-routed core ----
    public void set(byte[] key, byte[] value) {
        Decode.ok(requestKeyed(key, Bytes.of("SET"), key, value));
    }

    public void setWithTtl(byte[] key, byte[] value, Duration ttl) {
        Decode.ok(requestKeyed(key, Bytes.of("SET"), key, value, Bytes.of("PX"), Bytes.ofLong(Math.max(1, Bytes.toMs(ttl)))));
    }

    public Optional<byte[]> get(byte[] key) {
        return Optional.ofNullable(Decode.optBulk(requestKeyed(key, Bytes.of("GET"), key)));
    }

    public long incr(byte[] key) {
        return Decode.intVal(requestKeyed(key, Bytes.of("INCR"), key));
    }

    public long incrBy(byte[] key, long delta) {
        return Decode.intVal(requestKeyed(key, Bytes.of("INCRBY"), key, Bytes.ofLong(delta)));
    }

    public boolean expire(byte[] key, Duration ttl) {
        return Decode.bool(requestKeyed(key, Bytes.of("PEXPIRE"), key, Bytes.ofLong(Bytes.toMs(ttl))));
    }

    public boolean persist(byte[] key) {
        return Decode.bool(requestKeyed(key, Bytes.of("PERSIST"), key));
    }

    public long ttlMs(byte[] key) {
        return Decode.intVal(requestKeyed(key, Bytes.of("PTTL"), key));
    }

    // ---- per-key routed + summed ----
    public long del(byte[]... keys) {
        return perKeySum("DEL", keys);
    }

    public long exists(byte[]... keys) {
        return perKeySum("EXISTS", keys);
    }

    private long perKeySum(String verb, byte[][] keys) {
        long total = 0;
        for (byte[] k : keys) {
            long n = Decode.intVal(requestKeyed(k, Bytes.of(verb), k));
            if (n > 0) total += n;
        }
        return total;
    }

    // ---- keyless / whole-cluster ----
    public long dbsize() {
        return Decode.intVal(requestUnkeyed(Bytes.of("DBSIZE")));
    }

    public void flushall() {
        Decode.ok(requestUnkeyed(Bytes.of("FLUSHALL")));
    }

    // ---- a few key-routed collection ops (single-key) ----
    public long hset(byte[] key, byte[]... fieldValueFlat) {
        Argv a = Argv.cmd("HSET").add(key).addAll(fieldValueFlat);
        return Decode.intVal(shardFor(key).request(a.list()));
    }

    public Optional<byte[]> hget(byte[] key, byte[] field) {
        return Optional.ofNullable(Decode.optBulk(requestKeyed(key, Bytes.of("HGET"), key, field)));
    }

    public long lpush(byte[] key, byte[]... values) {
        Argv a = Argv.cmd("LPUSH").add(key).addAll(values);
        return Decode.intVal(shardFor(key).request(a.list()));
    }

    public long sadd(byte[] key, byte[]... members) {
        Argv a = Argv.cmd("SADD").add(key).addAll(members);
        return Decode.intVal(shardFor(key).request(a.list()));
    }

    public long zadd(byte[] key, double score, byte[] member) {
        return Decode.intVal(requestKeyed(key, Bytes.of("ZADD"), key, Bytes.ofDouble(score), member));
    }

    public OptionalDouble zscore(byte[] key, byte[] member) {
        Double d = Decode.optFloat(requestKeyed(key, Bytes.of("ZSCORE"), key, member));
        return d == null ? OptionalDouble.empty() : OptionalDouble.of(d);
    }

    /** Set intersection routed by the FIRST key; all keys must share a slot ({tag}). */
    public List<byte[]> sinter(byte[]... keys) {
        return setCombine("SINTER", keys);
    }

    public List<byte[]> sunion(byte[]... keys) {
        return setCombine("SUNION", keys);
    }

    public List<byte[]> sdiff(byte[]... keys) {
        return setCombine("SDIFF", keys);
    }

    private List<byte[]> setCombine(String verb, byte[][] keys) {
        if (keys.length == 0) throw new InvalidInputException("set combine needs ≥ 1 key");
        Argv a = Argv.cmd(verb).addAll(keys);
        return Decode.bulks(shardFor(keys[0]).request(a.list()));
    }

    @Override
    public void close() {
        for (RespConn c : shards) c.close();
    }

    // ---- CLUSTER SLOTS parsing + topology ----
    private static void buildTopology(List<SlotRange> ranges, List<Node> nodes, int[] table) {
        if (ranges.isEmpty()) throw new ProtocolException("CLUSTER SLOTS returned no ranges");
        for (SlotRange r : ranges) {
            int idx = -1;
            for (int i = 0; i < nodes.size(); i++) {
                Node n = nodes.get(i);
                if (n.host().equals(r.host()) && n.port() == r.port()) { idx = i; break; }
            }
            if (idx < 0) {
                nodes.add(new Node(r.host(), r.port()));
                idx = nodes.size() - 1;
            }
            for (int slot = r.start(); slot <= r.end(); slot++) table[slot] = idx;
        }
    }

    private static List<SlotRange> parseSlots(Reply reply) {
        List<Reply> rows = reply.items();
        if (rows == null) throw new ProtocolException("malformed CLUSTER SLOTS reply");
        List<SlotRange> out = new ArrayList<>(rows.size());
        for (Reply row : rows) {
            List<Reply> cells = row.items();
            if (cells == null || cells.size() < 3) throw new ProtocolException("malformed CLUSTER SLOTS row");
            long start = asInt(cells.get(0));
            long end = asInt(cells.get(1));
            List<Reply> node = cells.get(2).items();
            if (node == null || node.size() < 2) throw new ProtocolException("malformed CLUSTER SLOTS node");
            String host = Bytes.str(node.get(0).payload());
            long port = asInt(node.get(1));
            out.add(new SlotRange((int) start, (int) end, host, (int) port));
        }
        return out;
    }

    private static long asInt(Reply r) {
        if (r instanceof Reply.Int i) return i.value();
        throw new ProtocolException("expected integer in CLUSTER SLOTS, got " + r.shape());
    }
}

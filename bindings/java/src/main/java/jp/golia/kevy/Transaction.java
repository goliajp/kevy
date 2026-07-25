// Transaction — MULTI/EXEC/DISCARD + WATCH, remote-only (client-contract
// §3.12). Wire flow: MULTI → +OK; each queued command → +QUEUED; EXEC → an
// array of N typed replies (or Nil when a WATCH violation aborts the txn).
//
// AutoCloseable: a Transaction abandoned without exec/discard sends an
// implicit DISCARD on close(), so the socket is never left in MULTI mode
// (§3.12 Drop note) — use it with try-with-resources. It drives the single
// command connection and must not be interleaved with other commands on the
// same client from another thread.
package jp.golia.kevy;

import java.util.ArrayList;
import java.util.List;
import java.util.Optional;

public final class Transaction implements AutoCloseable {
    private final RespConn conn;
    private boolean live = true;

    private Transaction(RespConn conn) {
        this.conn = conn;
    }

    static void watch(KevyClient client, byte[][] keys) {
        RespConn conn = requireRemote(client, "WATCH");
        Decode.ok(conn.request(Argv.cmd("WATCH").addAll(keys).list()));
    }

    static void unwatch(KevyClient client) {
        RespConn conn = requireRemote(client, "UNWATCH");
        Decode.ok(conn.request(Argv.cmd("UNWATCH").list()));
    }

    static Transaction begin(KevyClient client) {
        RespConn conn = requireRemote(client, "MULTI");
        Decode.ok(conn.request(Argv.cmd("MULTI").list()));
        return new Transaction(conn);
    }

    private static RespConn requireRemote(KevyClient client, String feature) {
        if (client.isEmbedded()) throw new UnsupportedException(feature + " is remote-only (embedded access is already serial)");
        return client.backend().remoteConn();
    }

    /** Raw argv passthrough; expects +QUEUED. */
    public Transaction queue(byte[]... parts) {
        if (parts.length == 0) throw new InvalidInputException("queue needs at least a verb");
        Reply r = conn.request(new ArrayList<>(List.of(parts)));
        if (r.isError()) throw Errors.fromReplyText(r.payload());
        return this;
    }

    // Typed builders (§3.12) — each expects +QUEUED.
    public Transaction set(byte[] key, byte[] value) { return queue(Bytes.of("SET"), key, value); }
    public Transaction get(byte[] key) { return queue(Bytes.of("GET"), key); }
    public Transaction del(byte[]... keys) { return queue(prepend("DEL", keys)); }
    public Transaction exists(byte[]... keys) { return queue(prepend("EXISTS", keys)); }
    public Transaction incr(byte[] key) { return queue(Bytes.of("INCR"), key); }
    public Transaction incrBy(byte[] key, long delta) { return queue(Bytes.of("INCRBY"), key, Bytes.ofLong(delta)); }
    public Transaction mget(byte[]... keys) { return queue(prepend("MGET", keys)); }
    public Transaction mset(byte[]... kv) { return queue(prepend("MSET", kv)); }

    /** EXEC → N replies; a WATCH abort (Nil) collapses to an empty list (legacy). */
    public List<Reply> exec() {
        Optional<List<Reply>> r = execWatched();
        return r.orElseGet(ArrayList::new);
    }

    /** EXEC → replies, or empty Optional on a WATCH abort. */
    public Optional<List<Reply>> execWatched() {
        Reply r = doExec();
        if (r.isNil()) return Optional.empty();
        List<Reply> items = r.items();
        if (items == null) throw Errors.unexpected(r, "array");
        return Optional.of(items);
    }

    /** EXEC → typed cursor; a WATCH abort raises a Protocol error. */
    public TransactionReplies execTyped() {
        return execWatchedTyped().orElseThrow(() -> new ProtocolException("transaction aborted (WATCH)"));
    }

    /** EXEC → typed cursor, or empty Optional on a WATCH abort. */
    public Optional<TransactionReplies> execWatchedTyped() {
        return execWatched().map(TransactionReplies::new);
    }

    private Reply doExec() {
        Reply r = conn.request(Argv.cmd("EXEC").list());
        live = false;
        if (r.isError()) throw Errors.fromReplyText(r.payload());
        return r;
    }

    public void discard() {
        if (!live) return;
        live = false;
        Decode.ok(conn.request(Argv.cmd("DISCARD").list()));
    }

    @Override
    public void close() {
        discard();
    }

    private static byte[][] prepend(String verb, byte[][] rest) {
        byte[][] out = new byte[rest.length + 1][];
        out[0] = Bytes.of(verb);
        System.arraycopy(rest, 0, out, 1, rest.length);
        return out;
    }
}

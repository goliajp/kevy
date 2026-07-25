// RemoteBackend — the kevy:// / redis:// / tcp:// backend over a RESP TCP
// socket (client-contract §1.1). There is no scalar fast path on the wire, so
// fastGet/fastSet degrade to GET / SET … PX commands.
package jp.golia.kevy;

import java.util.ArrayList;
import java.util.List;

final class RemoteBackend implements Backend {
    private final RespConn conn;

    RemoteBackend(RespConn conn) {
        this.conn = conn;
    }

    @Override
    public Reply exec(List<byte[]> argv) {
        return conn.request(argv);
    }

    @Override
    public byte[] fastGet(byte[] key) {
        return Decode.optBulk(conn.request(List.of(Bytes.of("GET"), key)));
    }

    @Override
    public void fastSet(byte[] key, byte[] value, long ttlMs) {
        List<byte[]> argv = new ArrayList<>();
        argv.add(Bytes.of("SET"));
        argv.add(key);
        argv.add(value);
        if (ttlMs > 0) {
            argv.add(Bytes.of("PX"));
            argv.add(Bytes.ofLong(ttlMs));
        }
        Decode.ok(conn.request(argv));
    }

    @Override
    public boolean embedded() {
        return false;
    }

    @Override
    public EmbeddedDb embeddedDb() {
        return null;
    }

    @Override
    public RespConn remoteConn() {
        return conn;
    }

    @Override
    public void close() {
        conn.close();
    }
}

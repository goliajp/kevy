// EmbeddedBackend — the mem:// / file:// backend, over an in-process store
// leased from the process-global registry (client-contract §1.3, §5). The
// scalar fast paths bypass RESP framing entirely; close() releases the lease
// (the store closes when its last lease drops).
package jp.golia.kevy;

import java.util.List;

final class EmbeddedBackend implements Backend {
    private final Registry.Lease lease;

    EmbeddedBackend(Registry.Lease lease) {
        this.lease = lease;
    }

    @Override
    public Reply exec(List<byte[]> argv) {
        return lease.db().cmd(argv);
    }

    @Override
    public byte[] fastGet(byte[] key) {
        return lease.db().get(key);
    }

    @Override
    public void fastSet(byte[] key, byte[] value, long ttlMs) {
        lease.db().set(key, value, ttlMs);
    }

    @Override
    public boolean embedded() {
        return true;
    }

    @Override
    public EmbeddedDb embeddedDb() {
        return lease.db();
    }

    @Override
    public RespConn remoteConn() {
        return null;
    }

    @Override
    public void close() {
        Registry.release(lease);
    }
}

// SyncBackend — serializes access to the underlying backend on one lock, so
// the blocking face and the async face (which schedules on an executor) can
// share a single connection/store and still agree on results. Transactions
// and pipelines take the same lock for their multi-exchange sequences.
package jp.golia.kevy;

import java.util.List;
import java.util.concurrent.locks.ReentrantLock;

final class SyncBackend implements Backend {
    private final Backend inner;
    final ReentrantLock lock = new ReentrantLock();

    SyncBackend(Backend inner) {
        this.inner = inner;
    }

    @Override
    public Reply exec(List<byte[]> argv) {
        lock.lock();
        try {
            return inner.exec(argv);
        } finally {
            lock.unlock();
        }
    }

    @Override
    public byte[] fastGet(byte[] key) {
        lock.lock();
        try {
            return inner.fastGet(key);
        } finally {
            lock.unlock();
        }
    }

    @Override
    public void fastSet(byte[] key, byte[] value, long ttlMs) {
        lock.lock();
        try {
            inner.fastSet(key, value, ttlMs);
        } finally {
            lock.unlock();
        }
    }

    @Override
    public boolean embedded() {
        return inner.embedded();
    }

    @Override
    public EmbeddedDb embeddedDb() {
        return inner.embeddedDb();
    }

    @Override
    public RespConn remoteConn() {
        return inner.remoteConn();
    }

    @Override
    public void close() {
        lock.lock();
        try {
            inner.close();
        } finally {
            lock.unlock();
        }
    }
}

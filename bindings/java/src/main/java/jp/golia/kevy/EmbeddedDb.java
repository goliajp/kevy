// EmbeddedDb — the in-process Store, the desktop-JVM door onto the embedded
// engine over libkevy_jni (client-contract §5). One `cmd(argv)` path reaches
// every verb; `get`/`set` are the scalar fast paths; `subscribe` opens a
// polled subscription. This is also the public kevy-embedded surface (§5.2)
// that mem:// / file:// clients delegate to.
package jp.golia.kevy;

import java.nio.charset.StandardCharsets;
import java.util.List;

public final class EmbeddedDb implements AutoCloseable {
    private long handle;

    private EmbeddedDb(long handle) {
        this.handle = handle;
    }

    /** Open a persistent store rooted at `dir` (AOF + snapshot, restart-safe). */
    public static EmbeddedDb open(String dir) {
        long h = KevyNative.open(Bytes.of(dir));
        if (h == 0) throw new KevyIoException("failed to open embedded store at " + dir, null);
        return new EmbeddedDb(h);
    }

    /** Open a pure in-memory store — nothing survives the process. */
    public static EmbeddedDb openMem() {
        long h = KevyNative.openMem();
        if (h == 0) throw new KevyIoException("failed to open in-memory embedded store", null);
        return new EmbeddedDb(h);
    }

    /** The embedded engine version string. */
    public static String version() {
        byte[] v = KevyNative.version();
        return v == null ? "" : new String(v, StandardCharsets.UTF_8);
    }

    /** Run one command through the universal argv path; returns the decoded reply. */
    public Reply cmd(List<byte[]> argv) {
        ensureOpen();
        byte[] packed = KevyNative.pack(argv.toArray(new byte[0][]));
        byte[] out = KevyNative.cmd(handle, packed);
        if (out == null) throw new ProtocolException("embedded cmd misuse (null/empty argv)");
        return RespParser.decode(out);
    }

    /** Scalar fast-path GET: the raw value bytes, or null on a miss. */
    public byte[] get(byte[] key) {
        ensureOpen();
        return KevyNative.get(handle, key);
    }

    /** Scalar fast-path SET (ttlMs 0 = no expiry). */
    public void set(byte[] key, byte[] value, long ttlMs) {
        ensureOpen();
        int rc = KevyNative.set(handle, key, value, ttlMs);
        if (rc < 0) throw new KevyIoException("embedded set failed (rc=" + rc + ")", null);
    }

    /** Open a polled subscription handle on a channel (or glob pattern). */
    long subscribeHandle(byte[] chan, boolean pattern) {
        ensureOpen();
        long sub = KevyNative.subscribe(handle, chan, pattern);
        if (sub == 0) throw new KevyIoException("embedded subscribe failed", null);
        return sub;
    }

    @Override
    public void close() {
        if (handle != 0) {
            KevyNative.close(handle);
            handle = 0;
        }
    }

    private void ensureOpen() {
        if (handle == 0) throw new ClosedException("embedded store is closed");
    }
}

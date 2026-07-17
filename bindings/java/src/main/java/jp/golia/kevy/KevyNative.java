// KevyNative — the raw JNI surface over libkevy_jni (crates/kevy-jni), the
// general desktop-JVM door onto the embedded engine.
//
// This mirrors bindings/android/java/jp/golia/kevy/KevyNative.java exactly:
// the *same* cdylib (libkevy_jni) serves both Android and a desktop JVM, so
// the two doors declare the same `Java_jp_golia_kevy_KevyNative_*` natives.
// It is kept here so the desktop `jp.golia:kevy` artifact is self-contained.
//
// Handles are opaque longs (0 = failure/null); every payload is a byte[].
// cmd() takes argv packed into ONE flat byte[] (u32-LE length prefix per
// argument — see pack()), which keeps the native side down to four JNIEnv
// calls. Typed ergonomics live one floor up, in KevyClient / EmbeddedDb.
package jp.golia.kevy;

public final class KevyNative {
    private KevyNative() {}

    static {
        System.loadLibrary("kevy_jni");
    }

    /** Open a persistent store rooted at the UTF-8 path in dir; 0 = failure. */
    public static native long open(byte[] dir);

    /** Open a pure in-memory store — nothing survives the process; 0 = failure. */
    public static native long openMem();

    /**
     * open() with explicit durability/rewrite policy; dir null = in-memory.
     * opts is a long[6]: [0] fsync (0 everysec, 1 always, 2 no), [1] shards
     * (0 = engine default), [2] rewritePct (growth trigger, 0 = rule off),
     * [3] rewriteMinSize (growth rule's minimum size gate, bytes),
     * [4] rewriteBytes (absolute-size trigger, 0 = off),
     * [5] rewriteIntervalSecs (staleness trigger, 0 = off). Negatives clamp
     * to 0; a null opts means exactly open()'s defaults. 0 = failure,
     * including a wrong-length array.
     */
    public static native long openWith(byte[] dir, long[] opts);

    /**
     * Deterministic teardown: flush every shard's AOF (a REAL fsync), write
     * the feed continuity marker, then refuse every later write; reads stay
     * available, so the handle stays live — close it separately. Idempotent:
     * a signal handler exits with shutdown(db) then System.exit(0).
     * 0 = ok, -1 = misuse, -2 = I/O failure (store still usable; retry).
     */
    public static native int shutdown(long db);

    /** Close a store handle. Must not be used afterwards; 0 is a no-op. */
    public static native void close(long db);

    /**
     * Run one command; argv packed per {@link #pack}. Returns the
     * RESP-encoded reply (a protocol error like -ERR is still a reply);
     * null means the call itself was misused.
     */
    public static native byte[] cmd(long db, byte[] packedArgv);

    /**
     * Scalar fast-path GET: raw value bytes, or null on a miss. A store error
     * (GET on a non-string key, i.e. WRONGTYPE) is thrown as a
     * {@link ScalarGetSignal} — the lane can't fold a typed error into a
     * byte[], so {@link EmbeddedBackend} catches it and re-runs the framed GET.
     */
    public static native byte[] get(long db, byte[] key);

    /** Scalar fast-path SET, ttlMs 0 = no expiry; 0 = ok, negative = misuse. */
    public static native int set(long db, byte[] key, byte[] val, long ttlMs);

    /**
     * The boot-replay verdict of this handle's open, as a long[6]:
     * [0] replayedCommands, [1] replayedBytes, [2] elapsedMs,
     * [3] droppedBytes, [4] corrupt (0/1), [5] quarantineCount.
     * [3] &gt; 0 or [4] != 0 means the store recovered LESS than its files
     * held (the dropped region was quarantined next to the AOF). Null on
     * misuse. Typed ergonomics live one floor up.
     */
    public static native long[] openReport(long db);

    /** The engine version as UTF-8 bytes. */
    public static native byte[] version();

    /** Subscribe to one channel (a glob when pattern); 0 = failure. */
    public static native long subscribe(long db, byte[] chan, boolean pattern);

    /** Drain one pending pub/sub frame (RESP array bytes); null = queue empty. */
    public static native byte[] subNext(long sub);

    /** Block up to timeoutMs (0 = forever) for one pub/sub frame; null on
     *  timeout / bus-gone. Kernel park, not a busy poll. */
    public static native byte[] subWait(long sub, long timeoutMs);

    /** Close a subscription handle. Must not be used afterwards. */
    public static native void subClose(long sub);

    /** Pack argv for {@link #cmd}: per argument, u32-LE length then bytes. */
    public static byte[] pack(byte[]... argv) {
        int total = 0;
        for (byte[] a : argv) total += 4 + a.length;
        byte[] out = new byte[total];
        int p = 0;
        for (byte[] a : argv) {
            int n = a.length;
            out[p] = (byte) n;
            out[p + 1] = (byte) (n >>> 8);
            out[p + 2] = (byte) (n >>> 16);
            out[p + 3] = (byte) (n >>> 24);
            System.arraycopy(a, 0, out, p + 4, n);
            p += 4 + n;
        }
        return out;
    }
}

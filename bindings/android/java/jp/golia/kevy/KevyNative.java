// KevyNative — the raw JNI surface over libkevy_jni (crates/kevy-jni).
//
// Handles are opaque longs (0 = failure/null); every payload is a byte[].
// cmd() takes argv packed into ONE flat byte[] (u32-LE length prefix per
// argument — see pack()), which keeps the native side down to four JNIEnv
// calls. Typed ergonomics live one floor up, in the Kotlin shell
// (jp.golia.kevy.Kevy); this class is deliberately bare.
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
     * byte[], so {@code KevyDB.get} catches it and re-runs the framed GET,
     * where the typed WRONGTYPE {@code KevyException} surfaces.
     */
    public static native byte[] get(long db, byte[] key);

    /** Scalar fast-path SET, ttlMs 0 = no expiry; 0 = ok, negative = misuse. */
    public static native int set(long db, byte[] key, byte[] val, long ttlMs);

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

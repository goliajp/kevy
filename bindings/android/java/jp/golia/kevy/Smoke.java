// The JVM smoke — same contract as crates/kevy-ffi/examples/c/smoke.c.
// Opens a persistent store, exercises the command entry and the scalar
// fast path, round-trips pub/sub, then reopens the same directory and
// proves the data survived. Exits non-zero on the first lie.
//
// Build + run (from the repo root):
//   cargo build -p kevy-jni
//   javac -d /tmp/kevy-jvm bindings/android/java/jp/golia/kevy/*.java
//   java -Djava.library.path=target/debug -cp /tmp/kevy-jvm \
//        jp.golia.kevy.Smoke /tmp/kevy-smoke-data
package jp.golia.kevy;

import java.nio.charset.StandardCharsets;
import java.util.Arrays;

public final class Smoke {
    private Smoke() {}

    private static byte[] b(String s) {
        return s.getBytes(StandardCharsets.UTF_8);
    }

    private static byte[] cmd(long db, String... argv) {
        byte[][] raw = new byte[argv.length][];
        for (int i = 0; i < argv.length; i++) raw[i] = b(argv[i]);
        byte[] reply = KevyNative.cmd(db, KevyNative.pack(raw));
        if (reply == null) fail("cmd misuse: " + String.join(" ", argv));
        return reply;
    }

    private static void expect(byte[] got, String want, String what) {
        if (!Arrays.equals(got, b(want))) {
            fail(what + ": got " + new String(got, StandardCharsets.UTF_8));
        }
    }

    private static void fail(String msg) {
        System.err.println("FAIL " + msg);
        System.exit(1);
    }

    public static void main(String[] args) {
        if (args.length != 1) fail("usage: Smoke <data-dir>");
        long db = KevyNative.open(b(args[0]));
        if (db == 0) fail("open");

        expect(cmd(db, "SET", "smoke:k", "v1"), "+OK\r\n", "SET");
        expect(cmd(db, "GET", "smoke:k"), "$2\r\nv1\r\n", "GET");

        // scalar fast path round trip
        if (KevyNative.set(db, b("fast:k"), b("fv"), 0) != 0) fail("fast set");
        byte[] fv = KevyNative.get(db, b("fast:k"));
        if (fv == null || !Arrays.equals(fv, b("fv"))) fail("fast get");
        if (KevyNative.get(db, b("fast:none")) != null) fail("fast get miss");

        // unknown verb: a protocol error is a reply, not a misuse
        byte[] err = cmd(db, "NOSUCHVERB");
        if (err.length == 0 || err[0] != '-') fail("unknown verb");

        // pub/sub round trip: ack frame, then the message frame
        long sub = KevyNative.subscribe(db, b("c1"), false);
        if (sub == 0) fail("subscribe");
        if (KevyNative.subNext(sub) == null) fail("subscribe ack");
        expect(cmd(db, "PUBLISH", "c1", "hello"), ":1\r\n", "PUBLISH");
        byte[] frame = KevyNative.subNext(sub);
        if (frame == null) fail("message frame");
        expect(frame, "*3\r\n$7\r\nmessage\r\n$2\r\nc1\r\n$5\r\nhello\r\n", "frame");
        if (KevyNative.subNext(sub) != null) fail("drained");
        KevyNative.subClose(sub);

        // durability: close, reopen the same dir, the key is still there
        KevyNative.close(db);
        db = KevyNative.open(b(args[0]));
        if (db == 0) fail("reopen");
        expect(cmd(db, "GET", "smoke:k"), "$2\r\nv1\r\n", "GET after reopen");

        expect(cmd(db, "DEL", "smoke:k"), ":1\r\n", "DEL");
        KevyNative.close(db);

        System.out.println("smoke-jvm: ok");
    }
}

// EmbSub — the embedded pub/sub bus consumer (client-contract §3.11, §5.2).
// One JNI subscription handle per channel/pattern (the C-ABI pub/sub is
// per-channel). The desktop JNI door exposes only a non-blocking poll
// (subNext) — no kevy_sub_wait — so recv polls the handles round-robin on a
// short interval, honouring the read timeout. The store is held through the
// process registry so a publish on the same URL reaches here.
package jp.golia.kevy;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

final class EmbSub {
    private final EmbeddedDb db;
    private final Registry.Lease lease;
    private final List<Handle> handles = new ArrayList<>();
    private int rr;

    private record Handle(long sub, byte[] name, boolean pattern) {}

    EmbSub(Registry.Lease lease) {
        this.lease = lease;
        this.db = lease.db();
    }

    void add(byte[][] names, boolean pattern) {
        for (byte[] n : names) {
            long sub = db.subscribeHandle(n, pattern);
            handles.add(new Handle(sub, n.clone(), pattern));
        }
    }

    void remove(byte[][] names, boolean pattern) {
        List<Handle> keep = new ArrayList<>();
        for (Handle h : handles) {
            boolean drop = h.pattern() == pattern && (names.length == 0 || nameIn(names, h.name()));
            if (drop) KevyNative.subClose(h.sub());
            else keep.add(h);
        }
        handles.clear();
        handles.addAll(keep);
    }

    /** Block for the next frame; timeout=null waits forever, else TimedOut. */
    Reply recv(java.time.Duration timeout) {
        long end = timeout == null ? Long.MAX_VALUE : System.nanoTime() + timeout.toNanos();
        while (true) {
            byte[] frame = poll();
            if (frame != null) return RespParser.decode(frame);
            if (timeout != null && System.nanoTime() >= end) throw new TimedOutException("recv timed out");
            try {
                Thread.sleep(2);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                throw new ClosedException("recv interrupted");
            }
        }
    }

    /** Drain one queued frame from any handle, round-robin (no starvation). */
    private byte[] poll() {
        int n = handles.size();
        for (int i = 0; i < n; i++) {
            Handle h = handles.get((rr + i) % n);
            byte[] frame = KevyNative.subNext(h.sub());
            if (frame != null) {
                rr = (rr + i + 1) % n;
                return frame;
            }
        }
        return null;
    }

    void close() {
        for (Handle h : handles) KevyNative.subClose(h.sub());
        handles.clear();
        Registry.release(lease);
    }

    private static boolean nameIn(byte[][] names, byte[] n) {
        for (byte[] m : names) if (Arrays.equals(m, n)) return true;
        return false;
    }
}

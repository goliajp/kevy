// EmbSub — the embedded pub/sub bus consumer (client-contract §3.11, §5.2).
// One JNI subscription handle per channel/pattern (the C-ABI pub/sub is
// per-channel). The door exposes both a non-blocking poll (subNext) and a
// blocking, kernel-parking wait (subWait). recv() first drains anything
// queued, then parks: the common single-handle case blocks the kernel on that
// one handle (no busy poll, no latency floor), while many handles round-robin
// short-timeout parks so none starves — subWait blocks one handle at a time.
// The store is held through the process registry so a publish on the same URL
// reaches here.
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

    /** Many-handle park slice: cap one blocking wait so the others get serviced. */
    private static final long MULTI_SLICE_MS = 50;
    /** Infinite-wait re-park cap: keeps a torn-down bus detectable, not a spin. */
    private static final long PARK_CAP_MS = 250;

    /** Block for the next frame; timeout=null waits forever, else TimedOut. */
    Reply recv(java.time.Duration timeout) {
        long end = timeout == null ? -1 : System.nanoTime() + timeout.toNanos();
        while (true) {
            byte[] frame = poll();
            if (frame != null) return RespParser.decode(frame);
            long remainingMs = remainingMs(end);
            if (remainingMs == 0) throw new TimedOutException("recv timed out");
            byte[] parked = park(remainingMs);
            if (parked != null) return RespParser.decode(parked);
        }
    }

    /** Milliseconds left until `end`: -1 = wait forever, 0 = deadline passed. */
    private static long remainingMs(long end) {
        if (end < 0) return -1;
        long ns = end - System.nanoTime();
        return ns <= 0 ? 0 : Math.max(1, ns / 1_000_000);
    }

    /** Kernel-park for one frame. One handle parks directly (immediate wakeup
     *  on arrival); many round-robin a short-timeout park so none starves. */
    private byte[] park(long remainingMs) {
        int n = handles.size();
        if (n == 1) {
            return KevyNative.subWait(handles.get(0).sub(), remainingMs < 0 ? PARK_CAP_MS : remainingMs);
        }
        if (n > 1) {
            long slice = remainingMs < 0 ? MULTI_SLICE_MS : Math.min(remainingMs, MULTI_SLICE_MS);
            Handle h = handles.get(rr % n);
            rr = (rr + 1) % n;
            return KevyNative.subWait(h.sub(), slice);
        }
        // No handles subscribed yet: bounded idle wait until one is added.
        sleep(remainingMs < 0 ? PARK_CAP_MS : Math.min(remainingMs, PARK_CAP_MS));
        return null;
    }

    private static void sleep(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new ClosedException("recv interrupted");
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

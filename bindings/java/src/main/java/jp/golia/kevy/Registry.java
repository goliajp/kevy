// Registry — the process-global, URL-keyed embedded-store registry
// (client-contract §1.3). Two connects with the same mem://<name> or
// file://<canonical-path> resolve to the SAME backing store (and the same
// pub/sub bus), so an embedded publisher and a subscriber opened on the same
// URL find each other. Anonymous mem:// is always isolated.
//
// Java has no weak-value map with deterministic finalization, so — like the
// Go and Python ports — we refcount: the entry evicts (and the store closes)
// when the last lease is released.
package jp.golia.kevy;

import java.util.HashMap;
import java.util.Map;

final class Registry {
    private Registry() {}

    /** A borrowed reference to a (possibly shared) embedded store. */
    record Lease(EmbeddedDb db, String key) {}

    private static final Map<String, Entry> STORES = new HashMap<>();

    private static final class Entry {
        final EmbeddedDb db;
        int refs;

        Entry(EmbeddedDb db) {
            this.db = db;
            this.refs = 1;
        }
    }

    /** Acquire (opening or sharing) the store for a resolved target. */
    static Lease acquire(Url.Target target) {
        String key = target.registryKey();
        if (key == null) {
            // Anonymous / isolated: a private store, never registered.
            return new Lease(openFor(target), null);
        }
        synchronized (STORES) {
            Entry e = STORES.get(key);
            if (e != null) {
                e.refs++;
                return new Lease(e.db, key);
            }
            EmbeddedDb db = openFor(target);
            STORES.put(key, new Entry(db));
            return new Lease(db, key);
        }
    }

    /** Release a lease; the store closes when its last lease drops. */
    static void release(Lease lease) {
        if (lease.key() == null) {
            lease.db().close();
            return;
        }
        synchronized (STORES) {
            Entry e = STORES.get(lease.key());
            if (e == null) return;
            e.refs--;
            if (e.refs <= 0) {
                STORES.remove(lease.key());
                e.db.close();
            }
        }
    }

    private static EmbeddedDb openFor(Url.Target target) {
        if (target instanceof Url.MemAnon || target instanceof Url.MemNamed) return EmbeddedDb.openMem();
        if (target instanceof Url.FileStore f) return EmbeddedDb.open(f.path());
        throw new IllegalStateException("remote target has no embedded store");
    }
}

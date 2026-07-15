// Kevy — the package entry point (client-contract §1.1). `connect(url)` picks
// the backend from the URL scheme: mem:// / file:// open an embedded store
// (shared by name/path via the process-global registry), kevy:// / redis:// /
// tcp:// dial a remote RESP server (kevy:// and redis:// honour a trailing
// /db with one SELECT; tcp:// is raw and ignores it).
package jp.golia.kevy;

public final class Kevy {
    private Kevy() {}

    public static KevyClient connect(String url) {
        Url.Target t = Url.parse(url);
        if (t instanceof Url.Remote r) {
            RespConn conn = RespConn.dial(r.host(), r.port());
            try {
                if (!r.raw() && r.db() != null) conn.selectDb(r.db());
            } catch (RuntimeException e) {
                conn.close();
                throw e;
            }
            return new KevyClient(new RemoteBackend(conn), url);
        }
        Registry.Lease lease = Registry.acquire(t);
        return new KevyClient(new EmbeddedBackend(lease), url);
    }

    /** The embedded engine version (from libkevy_jni). */
    public static String embeddedVersion() {
        return EmbeddedDb.version();
    }
}

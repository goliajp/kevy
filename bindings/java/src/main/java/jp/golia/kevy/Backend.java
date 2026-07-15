// Backend — the one seam every typed command runs through. A KevyClient holds
// exactly one backend, chosen purely by the connect URL (client-contract
// §1.1): an embedded in-process store or a remote RESP socket. Typed methods
// build argv and call exec(); only backend-specific features (remote-only
// IDX/MULTI/pipeline, embedded scalar fast paths and the embedded bus) reach
// past this interface.
package jp.golia.kevy;

import java.util.List;

interface Backend {
    /** Run one command; returns the decoded reply (an error frame is data here). */
    Reply exec(List<byte[]> argv);

    /** Scalar GET fast path — raw value bytes or null on miss. */
    byte[] fastGet(byte[] key);

    /** Scalar SET fast path (ttlMs 0 = no expiry). */
    void fastSet(byte[] key, byte[] value, long ttlMs);

    boolean embedded();

    /** The embedded store (for the embedded pub/sub bus), or null when remote. */
    EmbeddedDb embeddedDb();

    /** The remote connection (for remote-only families), or null when embedded. */
    RespConn remoteConn();

    void close();
}

// HExpireCond — the optional condition on a hash-field TTL op (client-contract
// §3.7, §4.13). At most one; the wire keyword is emitted for the non-ALWAYS
// cases.
package jp.golia.kevy;

public enum HExpireCond {
    ALWAYS, NX, XX, GT, LT;

    /** The wire keyword, or null for ALWAYS (no keyword emitted). */
    byte[] wire() {
        return this == ALWAYS ? null : Bytes.of(name());
    }
}

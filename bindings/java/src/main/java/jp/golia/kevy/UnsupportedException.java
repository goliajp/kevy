// UnsupportedException — an op not available on this backend/build: IDX.* /
// MULTI / pipeline / hello3 on embedded, TLS, AUTH, embedded URL where only
// remote is allowed (client-contract §2.2 UNSUPPORTED).
package jp.golia.kevy;

public final class UnsupportedException extends KevyException {
    public UnsupportedException(String message) {
        super(ErrorKind.UNSUPPORTED, message);
    }
}

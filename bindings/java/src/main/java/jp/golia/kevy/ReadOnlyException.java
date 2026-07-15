// ReadOnlyException — a write rejected because the target is a read-only
// replica (client-contract §2.2 READ_ONLY).
package jp.golia.kevy;

public final class ReadOnlyException extends KevyException {
    public ReadOnlyException(String message) {
        super(ErrorKind.READ_ONLY, message);
    }
}

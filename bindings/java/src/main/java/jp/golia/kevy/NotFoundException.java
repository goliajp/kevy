// NotFoundException — a named object (index, view, key) doesn't exist
// (client-contract §2.2 NOT_FOUND).
package jp.golia.kevy;

public final class NotFoundException extends KevyException {
    public NotFoundException(String message) {
        super(ErrorKind.NOT_FOUND, message);
    }
}

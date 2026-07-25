// InvalidInputException — a bad argument to a typed API, rejected before
// touching any state (client-contract §2.2 INVALID_INPUT). Also the landing
// for an unknown URL scheme / bad port.
package jp.golia.kevy;

public final class InvalidInputException extends KevyException {
    public InvalidInputException(String message) {
        super(ErrorKind.INVALID_INPUT, message);
    }
}

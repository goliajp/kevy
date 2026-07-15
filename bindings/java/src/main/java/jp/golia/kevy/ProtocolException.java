// ProtocolException — a server -ERR reply whose text is not a recognized
// store-semantic error (text preserved verbatim), or a malformed / unexpected
// reply shape (client-contract §2.2 PROTOCOL).
package jp.golia.kevy;

public final class ProtocolException extends KevyException {
    public ProtocolException(String message) {
        super(ErrorKind.PROTOCOL, message);
    }
}

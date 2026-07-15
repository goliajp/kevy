// ClosedException — the connection / in-process bus is gone: a clean
// server-side close mid-read, or a command on a closed client
// (client-contract §2.2 CLOSED).
package jp.golia.kevy;

public final class ClosedException extends KevyException {
    public ClosedException(String message) {
        super(ErrorKind.CLOSED, message);
    }
}

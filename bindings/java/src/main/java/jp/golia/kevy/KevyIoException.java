// KevyIoException — an OS/transport failure (socket, file, AOF)
// (client-contract §2.2 IO). Wraps the underlying java.io.IOException.
package jp.golia.kevy;

public final class KevyIoException extends KevyException {
    public KevyIoException(String message, Throwable cause) {
        super(ErrorKind.IO, null, message, cause);
    }
}

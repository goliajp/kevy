// KevyException — the base of the error hierarchy (client-contract §2).
//
// Unchecked (RuntimeException) so the fluent typed API and the async
// CompletableFuture face aren't littered with `throws`; this matches the
// idiom of the community's Java Redis clients. Each variant is a concrete
// subclass (StoreException, ProtocolException, …) so callers can `catch` a
// specific kind, and every instance also exposes `kind()` (+ `storeError()`
// for STORE) so value-style branching works too.
package jp.golia.kevy;

public abstract class KevyException extends RuntimeException {
    private final ErrorKind kind;
    private final StoreError storeError; // non-null only when kind == STORE

    protected KevyException(ErrorKind kind, String message) {
        this(kind, null, message, null);
    }

    protected KevyException(ErrorKind kind, StoreError storeError, String message, Throwable cause) {
        super(message, cause);
        this.kind = kind;
        this.storeError = storeError;
    }

    public final ErrorKind kind() {
        return kind;
    }

    /** The structured store-semantic error when {@code kind() == STORE}, else null. */
    public final StoreError storeError() {
        return storeError;
    }

    public final boolean isKind(ErrorKind k) {
        return kind == k;
    }
}

// StoreException — a structured store-semantic error (WRONGTYPE, non-integer,
// overflow, …). Surfaces uniformly on embedded and remote (client-contract
// §2.2 STORE / §2.3). The specific variant is `storeError()`.
package jp.golia.kevy;

public final class StoreException extends KevyException {
    public StoreException(StoreError err, String message) {
        super(ErrorKind.STORE, err, message, null);
    }
}

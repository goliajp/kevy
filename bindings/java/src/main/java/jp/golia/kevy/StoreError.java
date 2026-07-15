// StoreError — the structured store-semantic errors carried inside a
// KevyException of kind STORE (client-contract §2.3, §4.13). These stay
// structured (never stringly) so a caller can distinguish, e.g., a
// wrong-type store error from a transport failure on both backends.
package jp.golia.kevy;

public enum StoreError {
    WRONG_TYPE, NOT_INTEGER, OVERFLOW, OUT_OF_RANGE, NO_SUCH_KEY, NOT_FLOAT, OUT_OF_MEMORY
}

// ErrorKind — the KevyError variant taxonomy (client-contract §2.2). Every
// KevyException carries one; conformance tests assert on it so the variant
// identity survives the value→exception mapping.
package jp.golia.kevy;

public enum ErrorKind {
    STORE, IO, PROTOCOL, READ_ONLY, INVALID_INPUT, NOT_FOUND, UNSUPPORTED, TIMED_OUT, CLOSED
}

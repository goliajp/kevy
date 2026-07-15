// Errors — the single place that turns a server error frame's wire text into
// a typed KevyException (client-contract §2.2). A recognized store-semantic
// error becomes a StoreException(StoreError); "READONLY …" becomes a
// ReadOnlyException; anything else is a ProtocolException with the text
// preserved verbatim. Shared by embedded and remote, so the conformance
// tests can assert the same variant on both backends.
package jp.golia.kevy;

import java.nio.charset.StandardCharsets;

final class Errors {
    private Errors() {}

    /** Classify a server error text into a structured StoreError, or null. */
    static StoreError classifyStore(String text) {
        String t = text;
        if (t.startsWith("WRONGTYPE")) return StoreError.WRONG_TYPE;
        if (t.startsWith("OOM")) return StoreError.OUT_OF_MEMORY;
        String low = t.toLowerCase();
        if (low.contains("is not an integer")) return StoreError.NOT_INTEGER;
        if (low.contains("is not a valid float")) return StoreError.NOT_FLOAT;
        if (low.contains("overflow")) return StoreError.OVERFLOW;
        if (low.contains("no such key")) return StoreError.NO_SUCH_KEY;
        if (low.contains("out of range")) return StoreError.OUT_OF_RANGE;
        return null;
    }

    /** Build the exception a `-ERR …` reply text maps to. */
    static KevyException fromReplyText(String text) {
        StoreError se = classifyStore(text);
        if (se != null) return new StoreException(se, text);
        if (text.startsWith("READONLY")) return new ReadOnlyException(text);
        return new ProtocolException(text);
    }

    static KevyException fromReplyText(byte[] text) {
        return fromReplyText(new String(text, StandardCharsets.UTF_8));
    }

    /** Throw if the reply is an error frame; otherwise return it unchanged. */
    static Reply checked(Reply r) {
        if (r.isError()) throw fromReplyText(r.payload());
        return r;
    }

    /** A protocol error for an unexpected reply shape. */
    static ProtocolException unexpected(Reply r, String want) {
        return new ProtocolException("expected " + want + ", got " + r.shape());
    }
}

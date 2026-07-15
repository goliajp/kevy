// Reply — the universal decoded RESP2/RESP3 value (client-contract §4.1).
//
// A sealed hierarchy: every wire shape kevy (or a Redis-compatible server)
// can emit is one record here, so callers can `switch` on the variant. The
// nine RESP3-only prefixes (Map/Set/Double/Boolean/Verbatim/BigNumber/Null/
// Push/BlobError) sit alongside the five RESP2 shapes. `Nil` is the RESP2
// null bulk/array ($-1 / *-1); `Null` is the RESP3 `_` null — kept distinct
// because the wire distinguishes them, though both mean "absent".
package jp.golia.kevy;

import java.util.List;

public sealed interface Reply
    permits Reply.Simple, Reply.Error, Reply.Int, Reply.Bulk, Reply.Nil,
            Reply.Array, Reply.Map, Reply.Set, Reply.Double, Reply.Bool,
            Reply.Verbatim, Reply.BigNumber, Reply.Null, Reply.Push,
            Reply.BlobError {

    /** Discriminant, for callers that prefer an enum switch over pattern-matching. */
    enum Kind {
        SIMPLE, ERROR, INT, BULK, NIL, ARRAY, MAP, SET, DOUBLE, BOOL,
        VERBATIM, BIG_NUMBER, NULL, PUSH, BLOB_ERROR
    }

    Kind kind();

    /** A single Map entry (RESP3 `%`). */
    record Entry(Reply key, Reply value) {}

    record Simple(byte[] value) implements Reply { public Kind kind() { return Kind.SIMPLE; } }
    record Error(byte[] text) implements Reply { public Kind kind() { return Kind.ERROR; } }
    record Int(long value) implements Reply { public Kind kind() { return Kind.INT; } }
    record Bulk(byte[] value) implements Reply { public Kind kind() { return Kind.BULK; } }
    record Nil() implements Reply { public Kind kind() { return Kind.NIL; } }
    record Array(List<Reply> items) implements Reply { public Kind kind() { return Kind.ARRAY; } }
    record Map(List<Entry> entries) implements Reply { public Kind kind() { return Kind.MAP; } }
    record Set(List<Reply> items) implements Reply { public Kind kind() { return Kind.SET; } }
    record Double(double value) implements Reply { public Kind kind() { return Kind.DOUBLE; } }
    record Bool(boolean value) implements Reply { public Kind kind() { return Kind.BOOL; } }
    record Verbatim(byte[] format, byte[] text) implements Reply { public Kind kind() { return Kind.VERBATIM; } }
    record BigNumber(byte[] text) implements Reply { public Kind kind() { return Kind.BIG_NUMBER; } }
    record Null() implements Reply { public Kind kind() { return Kind.NULL; } }
    record Push(List<Reply> items) implements Reply { public Kind kind() { return Kind.PUSH; } }
    record BlobError(byte[] text) implements Reply { public Kind kind() { return Kind.BLOB_ERROR; } }

    Nil NIL = new Nil();
    Null NULL = new Null();

    /** True for an `Error`/`BlobError` frame — a server-side error reply. */
    default boolean isError() {
        return this instanceof Error || this instanceof BlobError;
    }

    /** True for `Nil` or the RESP3 `Null` — an absent value. */
    default boolean isNil() {
        return this instanceof Nil || this instanceof Null;
    }

    /**
     * The byte payload of any string-ish frame (Simple/Bulk/Error/BigNumber/
     * BlobError/Verbatim), or null for the rest. Used by the typed decoders.
     */
    default byte[] payload() {
        if (this instanceof Simple s) return s.value();
        if (this instanceof Bulk b) return b.value();
        if (this instanceof Error e) return e.text();
        if (this instanceof BigNumber n) return n.text();
        if (this instanceof BlobError e) return e.text();
        if (this instanceof Verbatim v) return v.text();
        return null;
    }

    /** The list backing an Array/Set/Push, or null otherwise. */
    default List<Reply> items() {
        if (this instanceof Array a) return a.items();
        if (this instanceof Set s) return s.items();
        if (this instanceof Push p) return p.items();
        return null;
    }

    /** A short human name of the shape, for protocol-error diagnostics. */
    default String shape() {
        return kind().name().toLowerCase();
    }
}

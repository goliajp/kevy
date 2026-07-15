// TransactionReplies — a typed cursor over a successful EXEC's per-command
// replies (client-contract §4.10). Each next_* consumes one reply; a variant
// mismatch raises a Protocol error but the cursor still advances, so
// expect_empty (the arity gate) stays meaningful.
package jp.golia.kevy;

import java.util.ArrayList;
import java.util.List;
import java.util.Optional;

public final class TransactionReplies {
    private final List<Reply> replies;
    private int pos;

    TransactionReplies(List<Reply> replies) {
        this.replies = replies;
    }

    public int remaining() {
        return replies.size() - pos;
    }

    /** Arity gate: errors if any replies remain unconsumed. */
    public void expectEmpty() {
        if (remaining() > 0) throw new ProtocolException("transaction has " + remaining() + " unconsumed replies");
    }

    /** Escape hatch: the next raw reply. */
    public Reply raw() {
        return next();
    }

    public void nextOk() {
        Reply r = next();
        if (!(r instanceof Reply.Simple)) throw Errors.unexpected(r, "+OK");
    }

    /** +OK → true, Nil → false (for SET … NX/XX). */
    public boolean nextOkOrNil() {
        Reply r = next();
        if (r instanceof Reply.Simple) return true;
        if (r.isNil()) return false;
        throw Errors.unexpected(r, "+OK or nil");
    }

    public long nextInt() {
        Reply r = next();
        if (r instanceof Reply.Int i) return i.value();
        throw Errors.unexpected(r, "integer");
    }

    public Optional<byte[]> nextBulk() {
        Reply r = next();
        if (r.isNil()) return Optional.empty();
        if (r instanceof Reply.Bulk b) return Optional.of(b.value());
        throw Errors.unexpected(r, "bulk");
    }

    public List<byte[]> nextArrayOfBulks() {
        Reply r = next();
        List<Reply> items = r.items();
        if (items == null) throw Errors.unexpected(r, "array");
        List<byte[]> out = new ArrayList<>(items.size());
        for (Reply e : items) out.add(e.isNil() ? null : e.payload());
        return out;
    }

    public byte[] nextSimple() {
        Reply r = next();
        if (r instanceof Reply.Simple s) return s.value();
        throw Errors.unexpected(r, "simple string");
    }

    private Reply next() {
        if (pos >= replies.size()) throw new ProtocolException("transaction replies exhausted");
        return replies.get(pos++);
    }
}

// RespParser — the one pure RESP2/RESP3 codec, shared by every backend and
// both faces (client-contract §4.1, §5.2).
//
// Decoding is incremental: `parse(buf, off, end)` returns the decoded Reply
// plus how many bytes it consumed, or a `used == 0` sentinel meaning
// "need more bytes" — exactly the convention the Go/Python/Rust ports use,
// so the same function drives the self-contained embedded reply buffer and a
// streaming TCP read loop. Encoding emits the RESP multibulk command form.
package jp.golia.kevy;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.io.UncheckedIOException;
import java.util.ArrayList;
import java.util.List;

public final class RespParser {
    private RespParser() {}

    /** A decoded reply and the number of buffer bytes it consumed. */
    public record Parsed(Reply reply, int used) {}

    private static final Parsed NEED_MORE = new Parsed(null, 0);

    /** Decode one reply from buf[off..end); NEED_MORE (used==0) if incomplete. */
    public static Parsed parse(byte[] buf, int off, int end) {
        Cursor c = new Cursor(buf, off, end);
        Reply r = c.parseOne();
        if (r == null) return NEED_MORE;
        return new Parsed(r, c.pos - off);
    }

    /**
     * Decode a self-contained reply buffer (the embedded C-ABI path): the
     * buffer is guaranteed to hold exactly one complete reply.
     */
    public static Reply decode(byte[] buf) {
        Parsed p = parse(buf, 0, buf.length);
        if (p.used() == 0 || p.reply() == null) {
            throw new ProtocolException("incomplete RESP reply from embedded engine");
        }
        return p.reply();
    }

    /**
     * Encode a command's argv into the RESP multibulk wire form, straight into
     * `out` — no intermediate byte[]. Lengths are written as ASCII digits by a
     * direct itoa (no String/`getBytes` per number). Callers on a buffered
     * socket stream pass it their `OutputStream` and let the batching flush.
     */
    public static void encodeCommand(OutputStream out, List<byte[]> argv) throws IOException {
        writeLine(out, '*', argv.size());
        for (byte[] a : argv) {
            writeLine(out, '$', a.length);
            out.write(a);
            out.write('\r');
            out.write('\n');
        }
    }

    /** Encode into a fresh byte[] — for callers that want the bytes in hand. */
    public static byte[] encodeCommand(List<byte[]> argv) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        try {
            encodeCommand(out, argv);
        } catch (IOException e) {
            throw new UncheckedIOException(e); // ByteArrayOutputStream never throws
        }
        return out.toByteArray();
    }

    private static void writeLine(OutputStream out, char tag, long n) throws IOException {
        out.write(tag);
        writeDecimal(out, n);
        out.write('\r');
        out.write('\n');
    }

    /** itoa: write non-negative `n` as ASCII decimal digits, no allocation of a
     *  String. (RESP prefixes here — argv count, bulk length — are never
     *  negative.) */
    private static void writeDecimal(OutputStream out, long n) throws IOException {
        if (n == 0) {
            out.write('0');
            return;
        }
        byte[] tmp = new byte[20]; // Long.MAX_VALUE is 19 digits
        int i = tmp.length;
        while (n > 0) {
            tmp[--i] = (byte) ('0' + (int) (n % 10));
            n /= 10;
        }
        out.write(tmp, i, tmp.length - i);
    }

    /** A mutable position over the buffer; returns null anywhere it runs short. */
    private static final class Cursor {
        final byte[] buf;
        int pos;
        final int end;

        Cursor(byte[] buf, int pos, int end) {
            this.buf = buf;
            this.pos = pos;
            this.end = end;
        }

        Reply parseOne() {
            if (pos >= end) return null;
            byte tag = buf[pos];
            return switch (tag) {
                case '+' -> line(Reply.Simple::new);
                case '-' -> line(Reply.Error::new);
                case '(' -> line(Reply.BigNumber::new);
                case ':' -> intReply();
                case ',' -> doubleReply();
                case '#' -> boolReply();
                case '_' -> nullReply();
                case '$' -> bulk(false);
                case '!' -> bulk(true);
                case '=' -> verbatim();
                case '*' -> agg(Kind.ARRAY);
                case '~' -> agg(Kind.SET);
                case '>' -> agg(Kind.PUSH);
                case '%' -> mapReply();
                case '|' -> attributed();
                default -> throw new ProtocolException("unknown RESP tag: 0x" + Integer.toHexString(tag & 0xff));
            };
        }

        private enum Kind { ARRAY, SET, PUSH }

        /** Find the CRLF at or after `pos+1`; return the line end index or -1. */
        private int lineEnd() {
            for (int i = pos + 1; i + 1 < end; i++) {
                if (buf[i] == '\r' && buf[i + 1] == '\n') return i;
            }
            return -1;
        }

        private byte[] slice(int from, int to) {
            byte[] b = new byte[to - from];
            System.arraycopy(buf, from, b, 0, to - from);
            return b;
        }

        private Reply line(java.util.function.Function<byte[], Reply> make) {
            int le = lineEnd();
            if (le < 0) return null;
            byte[] body = slice(pos + 1, le);
            pos = le + 2;
            return make.apply(body);
        }

        private Reply intReply() {
            int le = lineEnd();
            if (le < 0) return null;
            long v = parseLong(pos + 1, le);
            pos = le + 2;
            return new Reply.Int(v);
        }

        private Reply doubleReply() {
            int le = lineEnd();
            if (le < 0) return null;
            String s = new String(buf, pos + 1, le - (pos + 1), java.nio.charset.StandardCharsets.US_ASCII);
            pos = le + 2;
            return new Reply.Double(parseDouble(s));
        }

        private Reply boolReply() {
            int le = lineEnd();
            if (le < 0) return null;
            boolean v = le > pos + 1 && buf[pos + 1] == 't';
            pos = le + 2;
            return new Reply.Bool(v);
        }

        private Reply nullReply() {
            int le = lineEnd();
            if (le < 0) return null;
            pos = le + 2;
            return Reply.NULL;
        }

        private Reply bulk(boolean blobError) {
            int le = lineEnd();
            if (le < 0) return null;
            long n = parseLong(pos + 1, le);
            if (n < 0) { pos = le + 2; return Reply.NIL; }
            int start = le + 2;
            int stop = start + (int) n;
            if (stop + 2 > end) return null;
            byte[] body = slice(start, stop);
            pos = stop + 2;
            return blobError ? new Reply.BlobError(body) : new Reply.Bulk(body);
        }

        private Reply verbatim() {
            int le = lineEnd();
            if (le < 0) return null;
            long n = parseLong(pos + 1, le);
            int start = le + 2;
            int stop = start + (int) n;
            if (n < 4 || stop + 2 > end) {
                if (stop + 2 > end) return null;
                throw new ProtocolException("malformed verbatim frame");
            }
            byte[] fmt = slice(start, start + 3); // "txt" etc, then ':'
            byte[] text = slice(start + 4, stop);
            pos = stop + 2;
            return new Reply.Verbatim(fmt, text);
        }

        private Reply agg(Kind k) {
            int le = lineEnd();
            if (le < 0) return null;
            long n = parseLong(pos + 1, le);
            pos = le + 2;
            if (n < 0) {
                if (k == Kind.PUSH) throw new ProtocolException("null push frame");
                return Reply.NIL;
            }
            int count = capHint(n);
            List<Reply> items = new ArrayList<>(count);
            for (int i = 0; i < n; i++) {
                Reply r = parseOne();
                if (r == null) return null;
                items.add(r);
            }
            return switch (k) {
                case ARRAY -> new Reply.Array(items);
                case SET -> new Reply.Set(items);
                case PUSH -> new Reply.Push(items);
            };
        }

        private Reply mapReply() {
            int le = lineEnd();
            if (le < 0) return null;
            long n = parseLong(pos + 1, le);
            pos = le + 2;
            if (n < 0) return Reply.NIL;
            int count = capHint(n);
            List<Reply.Entry> entries = new ArrayList<>(count);
            for (int i = 0; i < n; i++) {
                Reply key = parseOne();
                if (key == null) return null;
                Reply val = parseOne();
                if (val == null) return null;
                entries.add(new Reply.Entry(key, val));
            }
            return new Reply.Map(entries);
        }

        // RESP3 attribute frame: parse and discard the attribute map, then
        // return the reply it decorates.
        private Reply attributed() {
            int le = lineEnd();
            if (le < 0) return null;
            long n = parseLong(pos + 1, le);
            pos = le + 2;
            for (int i = 0; i < n; i++) {
                if (parseOne() == null) return null;
                if (parseOne() == null) return null;
            }
            return parseOne();
        }

        /** Cap a declared aggregate length by bytes remaining (fuzz-hardening). */
        private int capHint(long n) {
            long remaining = end - pos;
            return (int) Math.min(n, Math.max(0, remaining));
        }

        // atoi straight off the byte buffer — this fires on every `:` reply and
        // every bulk length header, so it avoids the String+trim+parseLong the
        // String path spent. Tolerates the surrounding ASCII whitespace the old
        // trim() did; an optional leading sign then decimal digits, nothing else.
        private long parseLong(int from, int to) {
            int i = from;
            while (i < to && (buf[i] == ' ' || buf[i] == '\t')) i++;
            int j = to;
            while (j > i && (buf[j - 1] == ' ' || buf[j - 1] == '\t')) j--;
            boolean neg = false;
            if (i < j && (buf[i] == '-' || buf[i] == '+')) {
                neg = buf[i] == '-';
                i++;
            }
            if (i >= j) throw badInt(from, to);
            long v = 0;
            for (; i < j; i++) {
                int d = buf[i] - '0';
                if (d < 0 || d > 9) throw badInt(from, to);
                if (v > (Long.MAX_VALUE - d) / 10) throw badInt(from, to); // overflow
                v = v * 10 + d;
            }
            return neg ? -v : v;
        }

        private ProtocolException badInt(int from, int to) {
            String s = new String(buf, from, to - from, java.nio.charset.StandardCharsets.US_ASCII);
            return new ProtocolException("bad integer in RESP frame: '" + s + "'");
        }

        private double parseDouble(String s) {
            return switch (s) {
                case "inf", "+inf" -> java.lang.Double.POSITIVE_INFINITY;
                case "-inf" -> java.lang.Double.NEGATIVE_INFINITY;
                case "nan" -> java.lang.Double.NaN;
                default -> {
                    try {
                        yield java.lang.Double.parseDouble(s);
                    } catch (NumberFormatException e) {
                        throw new ProtocolException("bad double in RESP frame: '" + s + "'");
                    }
                }
            };
        }
    }
}

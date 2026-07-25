// Decode — pure Reply→typed-value functions (client-contract §3). Every
// decoder first turns a server error frame into the right KevyException
// (Errors.checked), so embedded and remote, blocking and async, converge on
// identical typed results and identical error variants.
package jp.golia.kevy;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;

final class Decode {
    private Decode() {}

    static Void ok(Reply r) {
        Errors.checked(r);
        return null;
    }

    static long intVal(Reply r) {
        r = Errors.checked(r);
        if (r instanceof Reply.Int i) return i.value();
        throw Errors.unexpected(r, "integer");
    }

    static boolean bool(Reply r) {
        r = Errors.checked(r);
        if (r instanceof Reply.Int i) return i.value() != 0;
        if (r instanceof Reply.Bool b) return b.value();
        throw Errors.unexpected(r, "boolean/int");
    }

    static byte[] optBulk(Reply r) {
        r = Errors.checked(r);
        if (r.isNil()) return null;
        if (r instanceof Reply.Bulk b) return b.value();
        if (r instanceof Reply.Simple s) return s.value();
        throw Errors.unexpected(r, "bulk");
    }

    static String str(Reply r) {
        byte[] b = optBulk(r);
        return b == null ? null : new String(b, StandardCharsets.UTF_8);
    }

    /** An array of bulk strings (LRANGE/SMEMBERS/HKEYS/LPOP …). */
    static List<byte[]> bulks(Reply r) {
        r = Errors.checked(r);
        if (r.isNil()) return new ArrayList<>();
        List<Reply> items = r.items();
        if (items == null) throw Errors.unexpected(r, "array");
        List<byte[]> out = new ArrayList<>(items.size());
        for (Reply e : items) out.add(e.isNil() ? null : e.payload());
        return out;
    }

    /** An array of optional bulks, nulls preserved (MGET). */
    static List<byte[]> optBulks(Reply r) {
        return bulks(r);
    }

    /** A flat [k0,v0,k1,v1,…] map — RESP2 array or RESP3 map (HGETALL). */
    static List<byte[]> flatMap(Reply r) {
        r = Errors.checked(r);
        if (r instanceof Reply.Map m) {
            List<byte[]> out = new ArrayList<>(m.entries().size() * 2);
            for (Reply.Entry e : m.entries()) {
                out.add(e.key().payload());
                out.add(e.value().payload());
            }
            return out;
        }
        return bulks(r);
    }

    static Double optFloat(Reply r) {
        r = Errors.checked(r);
        if (r.isNil()) return null;
        if (r instanceof Reply.Double d) return d.value();
        byte[] b = r.payload();
        if (b == null) throw Errors.unexpected(r, "float");
        return parseFloat(b);
    }

    static double floatVal(Reply r) {
        Double d = optFloat(r);
        if (d == null) throw Errors.unexpected(r, "float");
        return d;
    }

    /** An array of integers (HTTL/HPTTL and, as i8 codes, HEXPIRE family). */
    static long[] intList(Reply r) {
        r = Errors.checked(r);
        List<Reply> items = r.items();
        if (items == null) throw Errors.unexpected(r, "int array");
        long[] out = new long[items.size()];
        for (int i = 0; i < items.size(); i++) {
            Reply e = items.get(i);
            if (e instanceof Reply.Int n) out[i] = n.value();
            else throw Errors.unexpected(e, "integer");
        }
        return out;
    }

    static double parseFloat(byte[] b) {
        String s = new String(b, StandardCharsets.US_ASCII).trim();
        return switch (s) {
            case "inf", "+inf" -> Double.POSITIVE_INFINITY;
            case "-inf" -> Double.NEGATIVE_INFINITY;
            case "nan" -> Double.NaN;
            default -> {
                try {
                    yield Double.parseDouble(s);
                } catch (NumberFormatException e) {
                    throw new ProtocolException("bad float: '" + s + "'");
                }
            }
        };
    }
}

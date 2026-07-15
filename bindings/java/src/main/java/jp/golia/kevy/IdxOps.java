// IdxOps — declarative secondary indexes, IDX.* (client-contract §3.8).
// Remote-only: the embedded backend answers Unsupported (its wire face coerces
// query bounds through the index's declared type server-side, which the client
// can't replicate without the catalog). Typed shortcuts plus *Raw argv
// passthroughs (queryRaw/createRaw) keep COMPOSE/HYBRID/GROUPS reachable.
package jp.golia.kevy;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;

final class IdxOps {
    private IdxOps() {}

    static void createRange(Backend b, String name, String prefix, String field, IdxType ty) {
        requireRemote(b);
        Argv a = Argv.cmd("IDX.CREATE").add(name).add("ON").add("PREFIX").add(prefix)
            .add("FIELD").add(field).add("TYPE").add(ty.wire()).add("KIND").add("range");
        Decode.ok(b.exec(a.list()));
    }

    static void createRaw(Backend b, List<byte[]> args) {
        requireRemote(b);
        Decode.ok(b.exec(Argv.cmd("IDX.CREATE").addAll(args).list()));
    }

    static boolean drop(Backend b, String name) {
        requireRemote(b);
        return Decode.bool(b.exec(Argv.cmd("IDX.DROP").add(name).list()));
    }

    static List<IdxInfo> list(Backend b) {
        requireRemote(b);
        Reply r = Errors.checked(b.exec(Argv.cmd("IDX.LIST").list()));
        List<Reply> items = r.items();
        if (items == null) throw Errors.unexpected(r, "array");
        List<IdxInfo> out = new ArrayList<>(items.size());
        for (Reply e : items) out.add(parseInfo(e));
        return out;
    }

    static IdxPage queryRange(Backend b, String name, byte[] min, byte[] max, long limit, byte[] cursor) {
        Argv a = new Argv().add(name).add("RANGE").add(min).add(max).add("LIMIT").addLong(limit);
        if (cursor != null) a.add("CURSOR").add(cursor);
        return parsePage(queryRaw(b, a.list()));
    }

    static IdxPage queryEq(Backend b, String name, byte[] value, long limit) {
        Argv a = new Argv().add(name).add("EQ").add(value).add("LIMIT").addLong(limit);
        return parsePage(queryRaw(b, a.list()));
    }

    static List<Scored> queryMatch(Backend b, String name, String text, long limit) {
        Argv a = new Argv().add(name).add("MATCH").add(text).add("LIMIT").addLong(limit);
        return parseRanked(queryRaw(b, a.list()));
    }

    static List<Scored> queryKnn(Backend b, String name, float[] vector, long k) {
        Argv a = new Argv().add(name).add("KNN").add(f32blob(vector)).add("LIMIT").addLong(k);
        return parseRanked(queryRaw(b, a.list()));
    }

    static Reply queryRaw(Backend b, List<byte[]> args) {
        requireRemote(b);
        return Errors.checked(b.exec(Argv.cmd("IDX.QUERY").addAll(args).list()));
    }

    private static void requireRemote(Backend b) {
        if (b.embedded()) throw new UnsupportedException("IDX.* is remote-only (use the embedded Store's typed idx API)");
    }

    /** f32 little-endian blob regardless of host endianness (§7). */
    private static byte[] f32blob(float[] v) {
        ByteBuffer buf = ByteBuffer.allocate(v.length * 4).order(ByteOrder.LITTLE_ENDIAN);
        for (float f : v) buf.putFloat(f);
        return buf.array();
    }

    private static IdxPage parsePage(Reply r) {
        List<Reply> top = r.items();
        if (top == null || top.size() != 2) throw new ProtocolException("IDX.QUERY page: expected [cursor, rows]");
        byte[] cursor = top.get(0).payload();
        if (cursor != null && new String(cursor, StandardCharsets.US_ASCII).equals("0")) cursor = null;
        List<Reply> flat = top.get(1).items();
        if (flat == null || flat.size() % 2 != 0) throw new ProtocolException("IDX.QUERY page: bad rows");
        List<IdxRow> rows = new ArrayList<>(flat.size() / 2);
        for (int i = 0; i < flat.size(); i += 2) {
            rows.add(new IdxRow(flat.get(i).payload(), flat.get(i + 1).payload()));
        }
        return new IdxPage(cursor, rows);
    }

    private static List<Scored> parseRanked(Reply r) {
        List<Reply> items = r.items();
        if (items == null) throw Errors.unexpected(r, "array");
        List<Scored> out = new ArrayList<>(items.size());
        for (Reply row : items) {
            List<Reply> cells = row.items();
            if (cells == null || cells.size() < 2) throw new ProtocolException("IDX.QUERY ranked row: expected [key, score]");
            out.add(new Scored(cells.get(0).payload(), Decode.floatVal(cells.get(1))));
        }
        return out;
    }

    private static IdxInfo parseInfo(Reply entry) {
        List<Reply> cells = entry.items();
        if (cells == null) throw Errors.unexpected(entry, "array");
        byte[] name = null, prefix = null;
        String kind = null, state = null;
        long entries = 0, bytes = 0;
        for (int i = 0; i + 1 < cells.size(); i += 2) {
            byte[] lb = cells.get(i).payload();
            Reply v = cells.get(i + 1);
            if (lb == null || v.payload() == null) continue;
            switch (new String(lb, StandardCharsets.US_ASCII)) {
                case "name" -> name = v.payload();
                case "prefix" -> prefix = v.payload();
                case "kind" -> kind = Bytes.str(v.payload());
                case "state" -> state = Bytes.str(v.payload());
                case "entries" -> entries = Long.parseLong(new String(v.payload(), StandardCharsets.US_ASCII).trim());
                case "bytes" -> bytes = Long.parseLong(new String(v.payload(), StandardCharsets.US_ASCII).trim());
                default -> { /* forward-compatible: skip unknown labels */ }
            }
        }
        return new IdxInfo(name, prefix, kind, state, entries, bytes);
    }
}

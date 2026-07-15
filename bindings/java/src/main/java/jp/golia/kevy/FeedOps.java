// FeedOps — change feed / CDC, FEED.* (client-contract §3.10). Both backends
// serve the same cursor contract, but this port's embedded Connect does not
// enable the change feed (the C-ABI kevy_open takes no feed flag), so embedded
// feed_tail/read answer Unsupported and feed_shards is always 1. Resync: an
// unservable cursor surfaces as a Protocol error whose text starts
// "FEEDRESYNC <gen> <tail>" — rebuild from a scan, resume from (gen, tail).
package jp.golia.kevy;

import java.util.ArrayList;
import java.util.List;

final class FeedOps {
    private FeedOps() {}

    static long shards(Backend b) {
        if (b.embedded()) return 1;
        return Decode.intVal(b.exec(Argv.cmd("FEED.SHARDS").list()));
    }

    static FeedTail tail(Backend b, long shard) {
        guardEmbedded(b, shard);
        Reply r = Errors.checked(b.exec(Argv.cmd("FEED.TAIL").addLong(shard).list()));
        List<Reply> a = r.items();
        if (a == null || a.size() != 2 || !(a.get(0) instanceof Reply.Int g) || !(a.get(1) instanceof Reply.Int n)) {
            throw new ProtocolException("FEED.TAIL: expected [gen, next]");
        }
        return new FeedTail(g.value(), n.value());
    }

    static FeedBatch read(Backend b, long shard, long generation, long offset, Long count, byte[][] prefixes) {
        guardEmbedded(b, shard);
        Argv a = Argv.cmd("FEED.READ").addLong(shard).addLong(generation).addLong(offset);
        if (count != null && count > 0) a.add("COUNT").addLong(count);
        for (byte[] p : prefixes) a.add("PREFIX").add(p);
        return parseBatch(Errors.checked(b.exec(a.list())));
    }

    private static void guardEmbedded(Backend b, long shard) {
        if (b.embedded()) {
            if (shard != 0) throw new InvalidInputException("embedded feed is single-shard: shard must be 0");
            throw new UnsupportedException("feed disabled: this port's embedded Connect does not enable the change feed");
        }
    }

    private static FeedBatch parseBatch(Reply r) {
        List<Reply> a = r.items();
        if (a == null || a.size() != 3 || !(a.get(0) instanceof Reply.Int g) || !(a.get(1) instanceof Reply.Int next)) {
            throw new ProtocolException("FEED.READ: expected [gen, next, frames]");
        }
        List<Reply> frames = a.get(2).items();
        if (frames == null) throw new ProtocolException("FEED.READ: frames not an array");
        List<FeedFrame> out = new ArrayList<>(frames.size());
        for (Reply f : frames) out.add(parseFrame(f));
        return new FeedBatch(g.value(), next.value(), out);
    }

    private static FeedFrame parseFrame(Reply f) {
        List<Reply> cells = f.items();
        if (cells == null || cells.size() != 2 || !(cells.get(0) instanceof Reply.Int off)) {
            throw new ProtocolException("FEED.READ: frame shape != [offset, argv]");
        }
        List<Reply> argvRaw = cells.get(1).items();
        if (argvRaw == null) throw new ProtocolException("FEED.READ: frame argv not an array");
        List<byte[]> argv = new ArrayList<>(argvRaw.size());
        for (Reply x : argvRaw) {
            byte[] p = x.payload();
            if (p == null) throw Errors.unexpected(x, "bulk");
            argv.add(p);
        }
        return new FeedFrame(off.value(), argv);
    }
}

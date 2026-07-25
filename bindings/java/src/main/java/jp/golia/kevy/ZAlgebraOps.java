// ZAlgebraOps — sorted-set algebra (client-contract §3.6). Available on both
// backends. Empty source keys → InvalidInput; WEIGHTS (if given) must be
// one-per-key; the AGGREGATE keyword is emitted only for a non-SUM mode.
package jp.golia.kevy;

final class ZAlgebraOps {
    private ZAlgebraOps() {}

    static long zinterstore(Backend b, byte[] dest, byte[][] keys) {
        return store(b, "ZINTERSTORE", dest, keys, null, ZAggregate.SUM);
    }

    static long zunionstore(Backend b, byte[] dest, byte[][] keys) {
        return store(b, "ZUNIONSTORE", dest, keys, null, ZAggregate.SUM);
    }

    static long zinterstoreWith(Backend b, byte[] dest, byte[][] keys, double[] weights, ZAggregate agg) {
        return store(b, "ZINTERSTORE", dest, keys, weights, agg);
    }

    static long zunionstoreWith(Backend b, byte[] dest, byte[][] keys, double[] weights, ZAggregate agg) {
        return store(b, "ZUNIONSTORE", dest, keys, weights, agg);
    }

    private static long store(Backend b, String verb, byte[] dest, byte[][] keys, double[] weights, ZAggregate agg) {
        requireKeys(keys);
        if (weights != null && weights.length != keys.length) {
            throw new InvalidInputException("WEIGHTS must be one-per-key");
        }
        Argv a = Argv.cmd(verb).add(dest).addLong(keys.length).addAll(keys);
        if (weights != null) {
            a.add("WEIGHTS");
            for (double w : weights) a.addDouble(w);
        }
        if (agg != null && agg != ZAggregate.SUM) {
            a.add("AGGREGATE").add(agg.wire());
        }
        return Decode.intVal(b.exec(a.list()));
    }

    static long zintercard(Backend b, byte[][] keys, Long limit) {
        requireKeys(keys);
        Argv a = Argv.cmd("ZINTERCARD").addLong(keys.length).addAll(keys);
        if (limit != null) a.add("LIMIT").addLong(limit);
        return Decode.intVal(b.exec(a.list()));
    }

    private static void requireKeys(byte[][] keys) {
        if (keys.length == 0) throw new InvalidInputException("needs at least one source key");
    }
}

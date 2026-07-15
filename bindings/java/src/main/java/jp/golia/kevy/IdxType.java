// IdxType — the declared type of a range index (client-contract §4.2).
package jp.golia.kevy;

public enum IdxType {
    I64("i64"), F64("f64"), STR("str");

    private final byte[] wire;

    IdxType(String w) {
        this.wire = Bytes.of(w);
    }

    byte[] wire() {
        return wire;
    }
}

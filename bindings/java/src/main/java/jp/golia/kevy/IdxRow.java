// IdxRow — one index hit: the matched key plus the indexed field's wire
// string form (client-contract §4.3).
package jp.golia.kevy;

public record IdxRow(byte[] key, byte[] value) {
    public String keyStr() { return Bytes.str(key); }
    public String valueStr() { return Bytes.str(value); }
}

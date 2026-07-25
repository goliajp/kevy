// KeyValue — a (key, value) pair, the result of a blocking pop
// (client-contract §3.14).
package jp.golia.kevy;

public record KeyValue(byte[] key, byte[] value) {
    public String keyStr() { return Bytes.str(key); }
    public String valueStr() { return Bytes.str(value); }
}

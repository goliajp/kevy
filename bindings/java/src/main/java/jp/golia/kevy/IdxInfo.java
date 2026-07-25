// IdxInfo — one IDX.LIST entry (client-contract §4.5). Parsed from a flat
// label/value bulk array; unknown labels are skipped (forward-compatible).
package jp.golia.kevy;

public record IdxInfo(byte[] name, byte[] prefix, String kind, String state, long entries, long bytes) {
    public String nameStr() { return Bytes.str(name); }
    public String prefixStr() { return Bytes.str(prefix); }
}

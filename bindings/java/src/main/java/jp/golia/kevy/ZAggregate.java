// ZAggregate — the AGGREGATE mode of a sorted-set store op (client-contract
// §3.6, §4.13). Default SUM; the wire keyword is emitted only for a
// non-default mode.
package jp.golia.kevy;

public enum ZAggregate {
    SUM, MIN, MAX;

    byte[] wire() {
        return Bytes.of(name());
    }
}

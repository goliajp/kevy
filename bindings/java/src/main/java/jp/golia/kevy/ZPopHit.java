// ZPopHit — the result of bzpopmin: a (key, member, score) triple
// (client-contract §3.14, §4.9).
package jp.golia.kevy;

public record ZPopHit(byte[] key, byte[] member, double score) {
    public String keyStr() { return Bytes.str(key); }
    public String memberStr() { return Bytes.str(member); }
}

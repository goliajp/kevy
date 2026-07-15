// ZMember — a (score, member) pair for ZADD (client-contract §3.5).
package jp.golia.kevy;

public record ZMember(double score, byte[] member) {
    public static ZMember of(double score, String member) {
        return new ZMember(score, Bytes.of(member));
    }
}

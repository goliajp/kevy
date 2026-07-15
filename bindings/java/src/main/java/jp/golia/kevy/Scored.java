// Scored — a (key, score) hit, returned by idx_query_match (BM25 score, best
// first) and idx_query_knn (distance, nearest first) (client-contract §3.8).
package jp.golia.kevy;

public record Scored(byte[] key, double score) {
    public String keyStr() { return Bytes.str(key); }
}

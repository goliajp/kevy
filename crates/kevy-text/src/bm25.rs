//! BM25 term scoring (Robertson/Spärck Jones idf variant), shard-local
//! statistics.

/// Term-frequency saturation.
pub const BM25_K1: f64 = 1.2;
/// Length normalization strength.
pub const BM25_B: f64 = 0.75;

/// One term's contribution for one document.
pub(crate) fn bm25_score(tf: f64, df: f64, n_docs: f64, dl: f64, avgdl: f64) -> f64 {
    let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
    let denom = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avgdl);
    idf * tf * (BM25_K1 + 1.0) / denom
}

/// Length-independent upper bound of one term's score (dl→0 floor of
/// the denominator) — the MaxScore pruning bound.
pub(crate) fn bm25_upper(max_tf: f64, df: f64, n_docs: f64) -> f64 {
    let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
    idf * max_tf * (BM25_K1 + 1.0) / (max_tf + BM25_K1 * (1.0 - BM25_B))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rarer_terms_score_higher_and_tf_saturates() {
        // Same tf/dl: rarer term (lower df) scores higher.
        let rare = bm25_score(1.0, 1.0, 100.0, 10.0, 10.0);
        let common = bm25_score(1.0, 50.0, 100.0, 10.0, 10.0);
        assert!(rare > common);
        // tf grows sub-linearly (saturation).
        let one = bm25_score(1.0, 5.0, 100.0, 10.0, 10.0);
        let ten = bm25_score(10.0, 5.0, 100.0, 10.0, 10.0);
        assert!(ten > one && ten < one * 10.0);
        // longer docs are normalized down.
        let short = bm25_score(2.0, 5.0, 100.0, 5.0, 10.0);
        let long = bm25_score(2.0, 5.0, 100.0, 40.0, 10.0);
        assert!(short > long);
        // idf never negative (the +1 variant).
        assert!(bm25_score(1.0, 99.0, 100.0, 10.0, 10.0) > 0.0);
    }
}

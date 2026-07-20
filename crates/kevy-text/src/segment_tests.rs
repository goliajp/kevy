//! Tests for [`crate::segment`] (child module via `#[path]`, so the
//! segment's private fields stay reachable).

use super::*;

fn seg() -> TextSegment {
    let mut s = TextSegment::new();
    s.apply(b"d1", Some("rust full text search engine".as_bytes()));
    s.apply(b"d2", Some("rust systems programming".as_bytes()));
    s.apply(b"d3", Some("全文检索引擎 rust 実装".as_bytes()));
    s
}

#[test]
fn ranked_or_semantics() {
    let s = seg();
    let hits = s.matches(b"rust search", 10);
    assert_eq!(hits.len(), 3, "OR semantics: every rust doc matches");
    assert_eq!(hits[0].key, b"d1".to_vec(), "d1 matches both terms → top");
    // rarer term dominates
    let hits = s.matches(b"programming", 10);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].key, b"d2".to_vec());
}

#[test]
fn cjk_query_bigrams() {
    let s = seg();
    let hits = s.matches("检索".as_bytes(), 10);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].key, b"d3".to_vec());
    assert!(s.matches("数据库".as_bytes(), 10).is_empty());
}

#[test]
fn update_and_remove() {
    let mut s = seg();
    s.apply(b"d1", Some(b"totally different now"));
    assert!(s.matches(b"engine", 10).is_empty(), "old tokens gone");
    assert_eq!(s.matches(b"different", 10)[0].key, b"d1".to_vec());
    s.apply(b"d2", None);
    assert!(!s.contains(b"d2"));
    assert!(s.matches(b"programming", 10).is_empty());
    let st = s.stats();
    assert_eq!(st.docs, 2);
    assert!(st.tokens > 0 && st.approx_bytes > 0);
}

#[test]
fn maxscore_pruning_matches_naive() {
    // df spread: "common" in every doc, "mid" in 1/5, "rare" in 2.
    let mut s = TextSegment::new();
    for i in 0..500u32 {
        let mut body = String::from("common filler words here");
        if i % 5 == 0 {
            body.push_str(" mid");
        }
        if i == 42 || i == 99 {
            body.push_str(" rare");
        }
        // vary length for dl normalization variety
        for _ in 0..(i % 7) {
            body.push_str(" pad");
        }
        s.apply(format!("k{i:03}").as_bytes(), Some(body.as_bytes()));
    }
    // Naive reference: walk everything.
    let naive = |query: &str, limit: usize| -> Vec<(Vec<u8>, f64)> {
        let q = tokenize(query.as_bytes());
        let n_docs = s.docs.len() as f64;
        let avgdl = s.total_len as f64 / n_docs;
        let mut sc: HashMap<Vec<u8>, f64> = HashMap::new();
        for t in &q {
            let Some(list) = s.postings.get(t) else { continue };
            let df = list.len() as f64;
            for (tf, bands) in list.tf_groups() {
                for (_b, band) in bands.iter() {
                    for &id in band {
                        let k = s.id_key[id as usize].clone().expect("live id");
                        let dl = f64::from(s.id_dl[id as usize]);
                        *sc.entry(k).or_insert(0.0) +=
                            bm25_score(f64::from(tf), df, n_docs, dl, avgdl);
                    }
                }
            }
        }
        let mut v: Vec<(Vec<u8>, f64)> = sc.into_iter().collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(limit);
        v
    };
    for (q, limit) in [("rare common", 10), ("mid common", 5), ("rare mid common", 3), ("common", 7)] {
        let got: Vec<(Vec<u8>, f64)> =
            s.matches(q.as_bytes(), limit).into_iter().map(|m| (m.key, m.score)).collect();
        let want = naive(q, limit);
        assert_eq!(got, want, "query {q:?} limit {limit}");
    }
}

#[test]
fn bucket_stop_keeps_walked_doc_contributions() {
    // Force the early stop: many tf=2 docs of a common term fill
    // the top-limit; the tf=1 bucket is skipped for NEW docs, but
    // a doc already accumulated via the rare term (sitting in
    // that tf=1 bucket) must still receive its contribution.
    let mut s = TextSegment::new();
    for i in 0..2000u32 {
        // "common common" → tf=2, short docs (strong scores)
        s.apply(format!("c{i:04}").as_bytes(), Some(b"common common"));
    }
    // the special doc: rare term + common ONCE (tf=1 bucket),
    // and a filler doc so `rare` df stays comparable
    s.apply(b"special", Some(b"rare common pad pad pad"));
    let naive_ok = {
        // by both-term score, special must beat every c-doc when
        // querying "rare common" (rare idf is huge)
        let hits = s.matches(b"rare common", 5);
        hits[0].key == b"special".to_vec()
    };
    assert!(naive_ok);
    // and its score must include the common-term part: compare
    // against a segment where special lacks "common".
    let mut s2 = TextSegment::new();
    for i in 0..2000u32 {
        s2.apply(format!("c{i:04}").as_bytes(), Some(b"common common"));
    }
    s2.apply(b"special", Some(b"rare only pad pad pad"));
    let with_common = s.matches(b"rare common", 1)[0].score;
    let without_common = s2.matches(b"rare common", 1)[0].score;
    assert!(
        with_common > without_common + 1e-9,
        "skipped-bucket contribution lost: {with_common} vs {without_common}"
    );
}

#[test]
fn limit_and_empty_query() {
    let s = seg();
    assert_eq!(s.matches(b"rust", 2).len(), 2);
    assert!(s.matches(b"", 10).is_empty());
    assert!(s.matches(b"!!!", 10).is_empty());
}

#[test]
fn limit_zero_is_empty() {
    // Fuzz finding: limit = 0 used to underflow in
    // `kth_of` (`limit - 1`) whenever the query hit a posting list.
    // Contract now: top-0 of anything is the empty ranking (same
    // convention as kevy-vector's `knn` with k = 0).
    let s = seg();
    assert!(s.matches(b"rust", 0).is_empty());
    assert!(s.matches(b"rust search engine", 0).is_empty());
    assert!(s.matches(b"", 0).is_empty());
    assert!(TextSegment::new().matches(b"rust", 0).is_empty());
}

// ---- weighted multi-field ------------------------------------------------

/// The reason multi-field is a struct change and not "make two indexes":
/// both fields score against ONE corpus, so their hits are comparable.
#[test]
fn a_weighted_field_outranks_the_same_term_in_a_plain_one() {
    let mut s = TextSegment::new();
    s.apply_fields(b"doc:title", Some(&[(b"rust".to_vec(), 3.0), (b"filler text here".to_vec(), 1.0)]));
    s.apply_fields(b"doc:body", Some(&[(b"other".to_vec(), 3.0), (b"rust filler text".to_vec(), 1.0)]));
    let hits = s.matches(b"rust", 10);
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].key, b"doc:title".to_vec(), "the weighted field must rank first");
    assert!(hits[0].score > hits[1].score);
}

/// Length normalisation is about how much text can dilute a match, so
/// weighting must not inflate it — otherwise boosting a field would
/// penalise the document carrying it.
#[test]
fn document_length_is_summed_unweighted() {
    let mut heavy = TextSegment::new();
    heavy.apply_fields(b"d", Some(&[(b"alpha beta".to_vec(), 5.0)]));
    let mut plain = TextSegment::new();
    plain.apply_fields(b"d", Some(&[(b"alpha beta".to_vec(), 1.0)]));
    assert_eq!(heavy.stats().docs, plain.stats().docs);
    // Same two tokens either way; only the term frequencies differ.
    assert_eq!(heavy.stats().tokens, plain.stats().tokens);
}

/// An update must remove exactly what it inserted. If removal re-derived
/// term frequencies at the wrong weight, postings would be left behind
/// and the term would keep matching a document that no longer has it.
#[test]
fn a_weighted_document_is_removed_exactly() {
    let mut s = TextSegment::new();
    s.apply_fields(b"d", Some(&[(b"alpha".to_vec(), 4.0), (b"beta".to_vec(), 2.0)]));
    assert_eq!(s.matches(b"alpha", 10).len(), 1);
    s.apply_fields(b"d", None);
    assert!(s.matches(b"alpha", 10).is_empty(), "alpha must not survive its document");
    assert!(s.matches(b"beta", 10).is_empty(), "beta must not survive either");
    assert_eq!(s.stats().tokens, 0, "no posting list may be left behind");
    assert_eq!(s.stats().postings, 0);
}

/// Re-indexing at a different weight must not double-count: the old
/// contribution has to be withdrawn at the old weight, not the new one.
#[test]
fn reindexing_at_a_new_weight_replaces_rather_than_accumulates() {
    let mut s = TextSegment::new();
    s.apply_fields(b"d", Some(&[(b"alpha".to_vec(), 5.0)]));
    let before = s.stats();
    s.apply_fields(b"d", Some(&[(b"alpha".to_vec(), 1.0)]));
    let after = s.stats();
    assert_eq!(after.docs, 1);
    assert_eq!(after.postings, before.postings, "one document, one posting");
}

/// A fractional weight de-emphasises without deleting: a term that
/// occurred is still a term that occurred.
#[test]
fn a_fractional_weight_never_rounds_a_hit_away() {
    let mut s = TextSegment::new();
    s.apply_fields(b"d", Some(&[(b"rare".to_vec(), 0.1)]));
    assert_eq!(s.matches(b"rare", 10).len(), 1, "0.1 weight must not erase the term");
}

/// The single-field entry point is sugar over the multi-field one, so a
/// plain apply and a neutral one-element apply_fields must be identical.
#[test]
fn apply_is_exactly_a_neutral_single_field() {
    let mut sugar = TextSegment::new();
    sugar.apply(b"d", Some(b"alpha beta alpha"));
    let mut explicit = TextSegment::new();
    explicit.apply_fields(b"d", Some(&[(b"alpha beta alpha".to_vec(), 1.0)]));
    let a = sugar.matches(b"alpha", 10);
    let b = explicit.matches(b"alpha", 10);
    assert_eq!(a.len(), b.len());
    assert_eq!(a[0].score, b[0].score);
    assert_eq!(sugar.stats().approx_bytes, explicit.stats().approx_bytes);
}

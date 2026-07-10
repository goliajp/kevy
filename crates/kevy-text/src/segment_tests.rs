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

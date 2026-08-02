//! [`super`]'s tests — the cold codec round-trips, the freeze
//! accounting, and the hot/cold parity pins (term scores, phrase
//! scores, highlight spans).

use super::*;
use crate::CorpusStats;

fn stats(n_docs: f64, avgdl: f64, df: &[(&[u8], u32)]) -> CorpusStats {
    CorpusStats {
        n_docs,
        avgdl,
        df: df.iter().map(|(t, d)| (t.to_vec(), *d)).collect(),
    }
}

fn seg_with_docs() -> TextSegment {
    let mut ts = TextSegment::new();
    for (key, text) in [
        (b"ev:1".as_slice(), b"rust engine fast".as_slice()),
        (b"ev:2", b"rust storage engine"),
        (b"ev:3", b"slow python glue"),
        (b"ev:4", b"rust rust rust everywhere"),
    ] {
        ts.apply_doc(key, Some(&[(text.to_vec(), 1.0)]), &[]);
    }
    ts
}

#[test]
fn codec_round_trips_and_refuses_garbage() {
    let entries = vec![
        ColdEntry { key: b"a\x00b".to_vec(), tf: 3, dl: 7, positions: vec![1, 2, 3] },
        ColdEntry { key: Vec::new(), tf: 1, dl: 1, positions: Vec::new() },
        ColdEntry { key: b"row:very-long-key-9999".to_vec(), tf: 200, dl: 4000, positions: vec![0; 40] },
    ];
    let payload = encode_posting(&entries);
    assert_eq!(posting_df(&payload), Some(3));
    let back = decode_posting(&payload).expect("decodes");
    assert_eq!(back.len(), 3);
    for (a, b) in entries.iter().zip(&back) {
        assert_eq!((&a.key, a.tf, a.dl, &a.positions), (&b.key, b.tf, b.dl, &b.positions));
    }
    assert!(decode_posting(&payload[..payload.len() - 1]).is_none(), "truncated");
    assert!(decode_posting(b"\xff\xff\xff\xff\xff").is_none(), "overlong varint");
}

#[test]
fn frozen_scores_equal_hot_scores_under_the_same_stats() {
    let mut ts = seg_with_docs();
    let st = stats(4.0, 3.25, &[(b"rust", 3), (b"engine", 2)]);
    // Hot scores for the docs we are about to freeze.
    let hot: std::collections::HashMap<Vec<u8>, f64> = ts
        .matches_scored(b"rust engine", 10, Some(&st))
        .into_iter()
        .map(|m| (m.key, m.score))
        .collect();

    let bucket = ts
        .freeze_docs(&[b"ev:1".to_vec(), b"ev:4".to_vec()])
        .expect("froze");
    assert_eq!(bucket.n_docs, 2);
    let mut acc = std::collections::HashMap::new();
    for term in [b"rust".as_slice(), b"engine"] {
        if let Some(p) = bucket.terms.get(term) {
            score_cold(p, term, &st, &|_| false, &mut acc).expect("scores");
        }
    }
    for key in [b"ev:1".as_slice(), b"ev:4"] {
        let cold = acc.get(key).copied().expect("cold hit");
        let hot = hot.get(key).copied().expect("hot hit");
        assert!(
            (cold - hot).abs() < 1e-12,
            "score drifted for {:?}: hot {hot} vs cold {cold}",
            String::from_utf8_lossy(key)
        );
    }
}

#[test]
fn freeze_reclaims_and_hot_queries_stop_seeing_frozen_docs() {
    let mut ts = seg_with_docs();
    let before = ts.stats().approx_bytes;
    let bucket = ts.freeze_docs(&[b"ev:1".to_vec(), b"ev:3".to_vec(), b"ev:9".to_vec()]).expect("froze");
    assert_eq!(bucket.n_docs, 2, "unknown key skipped");
    assert!(ts.stats().approx_bytes < before, "nothing reclaimed");
    assert_eq!(ts.docs(), 2);
    let st = stats(4.0, 3.25, &[(b"rust", 3)]);
    let hot: Vec<_> = ts.matches_scored(b"rust", 10, Some(&st)).into_iter().map(|m| m.key).collect();
    assert!(!hot.contains(&b"ev:1".to_vec()), "frozen doc still hot");
    assert!(hot.contains(&b"ev:2".to_vec()));
    // The bucket's terms are ascending — the segment builder's contract.
    let ts_keys: Vec<_> = bucket.terms.keys().cloned().collect();
    let mut sorted = ts_keys.clone();
    sorted.sort();
    assert_eq!(ts_keys, sorted);
}

#[test]
fn fwd_codec_round_trips_and_freeze_carries_exact_withdrawals() {
    let payload = encode_fwd(
        7,
        &[b"alpha".as_slice(), b"beta"],
        &[Some(b"42".as_slice()), None, Some(b"")],
    );
    let r = decode_fwd(&payload).expect("decodes");
    assert_eq!(r.dl, 7);
    assert_eq!(r.terms, vec![b"alpha".to_vec(), b"beta".to_vec()]);
    assert_eq!(r.values, vec![Some(b"42".to_vec()), None, Some(Vec::new())]);
    assert!(decode_fwd(&payload[..payload.len() - 1]).is_none(), "truncated");

    let mut ts = seg_with_docs();
    let bucket = ts.freeze_docs(&[b"ev:1".to_vec(), b"ev:4".to_vec()]).expect("froze");
    // ev:1 "rust engine fast" (dl 3), ev:4 "rust rust rust everywhere" (dl 4).
    let r1 = decode_fwd(bucket.fwd.get(b"ev:1".as_slice()).expect("fwd")).unwrap();
    assert_eq!(
        (r1.dl, r1.terms),
        (3, vec![b"engine".to_vec(), b"fast".to_vec(), b"rust".to_vec()])
    );
    let r4 = decode_fwd(bucket.fwd.get(b"ev:4".as_slice()).expect("fwd")).unwrap();
    assert_eq!((r4.dl, r4.terms), (4, vec![b"everywhere".to_vec(), b"rust".to_vec()]));
    // Withdrawing both restores an empty contribution exactly.
    assert_eq!(bucket.n_docs - 2, 0);
    assert_eq!(bucket.total_len - u64::from(r1.dl) - u64::from(r4.dl), 0);
}

fn positional_seg() -> TextSegment {
    let mut ts = TextSegment::with_positions();
    for (key, text) in [
        (b"ev:1".as_slice(), b"rust engine fast".as_slice()),
        (b"ev:2", b"rust storage engine"),
        (b"ev:3", b"the rust engine wins"),
        (b"ev:4", b"engine rust reversed"),
    ] {
        ts.apply_doc(key, Some(&[(text.to_vec(), 1.0)]), &[]);
    }
    ts
}

#[test]
fn frozen_phrase_scores_equal_hot_phrase_scores_under_the_same_stats() {
    let mut ts = positional_seg();
    let st = stats(4.0, 3.25, &[(b"rust", 4), (b"engine", 4)]);
    let hot: std::collections::HashMap<Vec<u8>, f64> = ts
        .phrase_matches(b"\"rust engine\"", 10, Some(&st))
        .into_iter()
        .map(|m| (m.key, m.score))
        .collect();
    // ev:1 and ev:3 hold the adjacent in-order pair; ev:4 reversed.
    assert!(hot.contains_key(b"ev:1".as_slice()) && hot.contains_key(b"ev:3".as_slice()));
    assert!(!hot.contains_key(b"ev:4".as_slice()));

    let bucket = ts
        .freeze_docs(&[b"ev:1".to_vec(), b"ev:3".to_vec(), b"ev:4".to_vec()])
        .expect("froze");
    let toks = vec![b"rust".to_vec(), b"engine".to_vec()];
    let payloads: Vec<Vec<u8>> =
        toks.iter().map(|t| bucket.terms.get(t).expect("term").clone()).collect();
    let mut acc = std::collections::HashMap::new();
    score_cold_phrase(&payloads, &toks, &st, &|_| false, &mut acc).expect("scores");
    assert!(!acc.contains_key(b"ev:4".as_slice()), "reversed pair scored");
    for key in [b"ev:1".as_slice(), b"ev:3"] {
        let cold = acc.get(key).copied().expect("cold phrase hit");
        let hot = hot.get(key).copied().expect("hot phrase hit");
        assert!(
            (cold - hot).abs() < 1e-12,
            "phrase score drifted for {:?}: hot {hot} vs cold {cold}",
            String::from_utf8_lossy(key)
        );
    }
}

#[test]
fn phrase_without_positions_contributes_nothing_cold_too() {
    let mut ts = seg_with_docs(); // no positions channel
    let bucket = ts.freeze_docs(&[b"ev:1".to_vec(), b"ev:2".to_vec()]).expect("froze");
    let toks = vec![b"rust".to_vec(), b"engine".to_vec()];
    let payloads: Vec<Vec<u8>> =
        toks.iter().map(|t| bucket.terms.get(t).expect("term").clone()).collect();
    let st = stats(4.0, 3.25, &[(b"rust", 3), (b"engine", 2)]);
    let mut acc = std::collections::HashMap::new();
    score_cold_phrase(&payloads, &toks, &st, &|_| false, &mut acc).expect("decodes");
    assert!(acc.is_empty(), "verified a phrase with no positions");
}

#[test]
fn cold_highlight_matches_the_hot_spans_for_the_same_text() {
    let ts = {
        let mut ts = TextSegment::new();
        ts.apply_doc(b"ev:1", Some(&[(b"rust engine goes fast, rust wins".to_vec(), 1.0)]), &[]);
        ts
    };
    for query in [b"rust".as_slice(), b"\"rust engine\"", b"fast rust"] {
        let hot = ts.highlight_spans(b"ev:1", query);
        let cold = highlight_fields(&[b"rust engine goes fast, rust wins".to_vec()], query);
        assert_eq!(hot, cold, "spans drifted for {:?}", String::from_utf8_lossy(query));
    }
}

#[test]
fn freeze_carries_stored_values_absent_and_present() {
    let mut ts = TextSegment::with_shape(crate::SegmentShape {
        fields: 0,
        positions: false,
        values: 2,
    });
    ts.apply_doc(b"ev:1", Some(&[(b"rust engine".to_vec(), 1.0)]), &[Some(b"9"), None]);
    ts.apply_doc(b"ev:2", Some(&[(b"rust glue".to_vec(), 1.0)]), &[Some(b"2"), Some(b"x")]);
    let bucket = ts.freeze_docs(&[b"ev:1".to_vec(), b"ev:2".to_vec()]).expect("froze");
    let r1 = decode_fwd(bucket.fwd.get(b"ev:1".as_slice()).unwrap()).unwrap();
    assert_eq!(r1.values, vec![Some(b"9".to_vec()), None]);
    let r2 = decode_fwd(bucket.fwd.get(b"ev:2".as_slice()).unwrap()).unwrap();
    assert_eq!(r2.values, vec![Some(b"2".to_vec()), Some(b"x".to_vec())]);
}

#[test]
fn tombstoned_rows_are_skipped() {
    let mut ts = seg_with_docs();
    let bucket = ts.freeze_docs(&[b"ev:1".to_vec(), b"ev:4".to_vec()]).expect("froze");
    let st = stats(4.0, 3.25, &[(b"rust", 3)]);
    let mut acc = std::collections::HashMap::new();
    let p = bucket.terms.get(b"rust".as_slice()).expect("term");
    score_cold(p, b"rust", &st, &|k| k == b"ev:4", &mut acc).expect("scores");
    assert!(acc.contains_key(b"ev:1".as_slice()));
    assert!(!acc.contains_key(b"ev:4".as_slice()), "tombstoned row scored");
}

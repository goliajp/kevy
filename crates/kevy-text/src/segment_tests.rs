//! Tests for [`crate::segment`] (child module via `#[path]`, so the
//! segment's private fields stay reachable).

use super::*;
// The naive MaxScore reference scores by hand; `bm25_score` now lives in
// the query submodule, so import it directly rather than via `super`.
use crate::bm25::bm25_score;

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

// ---- injected corpus stats (global BM25, step 4a) ------------------------

use std::collections::HashMap;

/// A CorpusStats summed by hand from what the whole corpus should look
/// like — the job step 4b's cross-shard aggregation will do for real.
fn global_of(segments: &[&TextSegment], q_tokens: &[&[u8]]) -> CorpusStats {
    let n_docs: f64 = segments.iter().map(|s| s.stats().docs as f64).sum();
    let total_len: f64 = segments.iter().map(|s| s.total_len() as f64).sum();
    let mut df = HashMap::new();
    for t in q_tokens {
        let d: u32 = segments.iter().map(|s| s.local_df(t)).sum();
        df.insert(t.to_vec(), d);
    }
    CorpusStats { n_docs, avgdl: total_len / n_docs, df }
}

/// The property step 4a exists for: a document scored against global
/// stats gets the same score whether it sits alone in one segment or in
/// a segment holding half the corpus. If it did not, shard count would
/// change ranking — the shard-local defect global BM25 removes.
#[test]
fn global_stats_make_split_and_whole_score_identically() {
    let docs: &[(&[u8], &str)] = &[
        (b"d1", "rust systems programming language rust"),
        (b"d2", "kevy pure rust key value store"),
        (b"d3", "the quick brown fox jumps"),
        (b"d4", "rust memory safety without garbage collection"),
        (b"d5", "a document with no query terms at all here"),
        (b"d6", "rust rust rust and more rust"),
    ];
    // Whole corpus in one segment.
    let mut whole = TextSegment::new();
    for (k, t) in docs {
        whole.apply(k, Some(t.as_bytes()));
    }
    // Same corpus split across two.
    let mut a = TextSegment::new();
    let mut b = TextSegment::new();
    for (i, (k, t)) in docs.iter().enumerate() {
        if i % 2 == 0 { &mut a } else { &mut b }.apply(k, Some(t.as_bytes()));
    }

    let q: &[&[u8]] = &[b"rust"];
    let g_whole = global_of(&[&whole], q);
    let g_split = global_of(&[&a, &b], q);
    // The two global views must agree — same corpus, same numbers.
    assert_eq!(g_whole.n_docs, g_split.n_docs);
    assert_eq!(g_whole.df.get(b"rust".as_slice()), g_split.df.get(b"rust".as_slice()));

    let whole_hits = whole.matches_scored(b"rust", 10, Some(&g_whole));
    let mut split_hits = a.matches_scored(b"rust", 10, Some(&g_split));
    split_hits.extend(b.matches_scored(b"rust", 10, Some(&g_split)));
    split_hits.sort_by(|x, y| y.score.total_cmp(&x.score).then_with(|| x.key.cmp(&y.key)));

    assert_eq!(whole_hits.len(), split_hits.len(), "same documents match");
    for (w, s) in whole_hits.iter().zip(&split_hits) {
        assert_eq!(w.key, s.key, "same ranking order");
        assert!((w.score - s.score).abs() < 1e-9, "same score for {:?}", w.key);
    }
}

/// The no-stats path must be byte-identical to before — this is the
/// regression that adding the parameter changed nothing for existing
/// callers.
#[test]
fn no_stats_matches_the_local_path() {
    let mut s = TextSegment::new();
    for (i, t) in ["rust here", "rust and rust", "nothing"].iter().enumerate() {
        s.apply(format!("d{i}").as_bytes(), Some(t.as_bytes()));
    }
    let a = s.matches(b"rust", 10);
    let b = s.matches_scored(b"rust", 10, None);
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.key, y.key);
        assert_eq!(x.score, y.score);
    }
}

// ---- positional index (phrase, step 5) -----------------------------------

/// Three documents whose offsets pin down adjacency: `d1` has
/// "quick brown" consecutive, `d2` has both words but far apart, `d3`
/// has them in the reverse order.
fn phrase_seg() -> TextSegment {
    let mut s = TextSegment::with_positions();
    s.apply(b"d1", Some(b"the quick brown fox jumps"));
    s.apply(b"d2", Some(b"quick red and then a brown hare"));
    s.apply(b"d3", Some(b"a brown quick animal appears"));
    s
}

/// A multi-token phrase matches only documents whose tokens are adjacent
/// and in order — the whole reason positions exist.
#[test]
fn phrase_requires_adjacency_and_order() {
    let s = phrase_seg();
    let hits = s.phrase_matches(b"quick brown", 10, None);
    assert_eq!(hits.len(), 1, "only d1 has quick-brown consecutive");
    assert_eq!(hits[0].key, b"d1".to_vec());
    // Reverse order is a different phrase.
    let rev = s.phrase_matches(b"brown quick", 10, None);
    assert_eq!(rev.len(), 1);
    assert_eq!(rev[0].key, b"d3".to_vec());
    // Words present but not adjacent → no phrase in any doc.
    assert!(s.phrase_matches(b"quick fox", 10, None).is_empty());
}

/// Without `WITH POSITIONS` a multi-token phrase cannot be verified, so
/// it returns empty rather than silently answering an OR query.
#[test]
fn phrase_without_positions_is_empty() {
    let mut s = TextSegment::new();
    s.apply(b"d1", Some(b"the quick brown fox"));
    assert!(!s.has_positions());
    assert!(s.phrase_matches(b"quick brown", 10, None).is_empty());
    // A single-token phrase is an ordinary term query and needs none.
    let single = s.phrase_matches(b"quick", 10, None);
    assert_eq!(single, s.matches(b"quick", 10));
}

/// A one-token phrase degrades to the ordinary term query, positions or
/// not — same hits, same scores.
#[test]
fn single_token_phrase_is_a_term_query() {
    let s = phrase_seg();
    let phrase = s.phrase_matches(b"brown", 10, None);
    let term = s.matches(b"brown", 10);
    assert_eq!(phrase, term);
    assert_eq!(phrase.len(), 3, "every doc has brown");
}

/// Recording positions must not change ranking: a `WITH POSITIONS`
/// segment answers ordinary `matches` byte-identically to a plain one.
#[test]
fn positions_do_not_change_ranking() {
    let docs: &[(&[u8], &str)] = &[
        (b"d1", "the quick brown fox jumps"),
        (b"d2", "quick red and then a brown hare"),
        (b"d3", "a brown quick animal appears"),
        (b"d4", "quick quick quick brown"),
    ];
    let mut plain = TextSegment::new();
    let mut pos = TextSegment::with_positions();
    for (k, t) in docs {
        plain.apply(k, Some(t.as_bytes()));
        pos.apply(k, Some(t.as_bytes()));
    }
    for q in [&b"quick"[..], b"brown", b"quick brown fox"] {
        assert_eq!(plain.matches(q, 10), pos.matches(q, 10), "ranking for {q:?}");
    }
    // The only stats difference is the positions side-channel's bytes.
    assert!(pos.stats().approx_bytes > plain.stats().approx_bytes);
    assert_eq!(pos.stats().docs, plain.stats().docs);
    assert_eq!(pos.stats().postings, plain.stats().postings);
}

/// Re-indexing must withdraw the old positions, not leave a phantom
/// phrase behind.
#[test]
fn reindexing_withdraws_old_positions() {
    let mut s = TextSegment::with_positions();
    s.apply(b"d", Some(b"the quick brown fox"));
    assert_eq!(s.phrase_matches(b"quick brown", 10, None).len(), 1);
    // Replace with text where the phrase no longer occurs.
    s.apply(b"d", Some(b"slow green turtle"));
    assert!(
        s.phrase_matches(b"quick brown", 10, None).is_empty(),
        "old positions must not survive the update"
    );
    assert_eq!(s.phrase_matches(b"green turtle", 10, None)[0].key, b"d".to_vec());
    // Removal clears positions entirely.
    s.apply(b"d", None);
    assert!(s.phrase_matches(b"green turtle", 10, None).is_empty());
}

/// A repeated word in the phrase ("very very good") must be verified with
/// two distinct adjacent occurrences, not one counted twice.
#[test]
fn phrase_with_repeated_token() {
    let mut s = TextSegment::with_positions();
    s.apply(b"twice", Some(b"it was very very good indeed"));
    s.apply(b"once", Some(b"it was very good indeed"));
    let hits = s.phrase_matches(b"very very", 10, None);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].key, b"twice".to_vec());
}

// ---- query grammar: bare terms + quoted phrases (step 5d) ----------------

/// A wholly-quoted query is exactly the phrase primitive.
#[test]
fn matches_query_pure_phrase_equals_phrase_matches() {
    let s = phrase_seg();
    let via_query = s.matches_query(b"\"quick brown\"", 10, None);
    let via_phrase = s.phrase_matches(b"quick brown", 10, None);
    assert_eq!(via_query, via_phrase);
    assert_eq!(via_query.len(), 1);
    assert_eq!(via_query[0].key, b"d1".to_vec());
}

/// A query with no quotes is byte-identical to the ordinary term query —
/// the pruned hot path is untouched when no phrase is present.
#[test]
fn matches_query_without_quotes_delegates_to_matches_scored() {
    let s = phrase_seg();
    for q in [&b"quick"[..], b"quick brown", b"brown fox jumps"] {
        assert_eq!(s.matches_query(q, 10, None), s.matches_scored(q, 10, None), "query {q:?}");
    }
}

/// A mixed query ORs its clauses: a bare term matches its documents, a
/// phrase matches only the adjacent ones, and a document satisfying both
/// scores higher than one satisfying only the term.
#[test]
fn matches_query_mixes_terms_and_phrases() {
    let mut s = TextSegment::with_positions();
    s.apply(b"both", Some(b"the quick brown fox and a jumps word"));
    s.apply(b"term_only", Some(b"jumps over something unrelated here"));
    s.apply(b"phrase_only", Some(b"a quick brown hare"));
    // Query: bare "jumps" OR phrase "quick brown".
    let hits = s.matches_query(b"jumps \"quick brown\"", 10, None);
    let keys: Vec<_> = hits.iter().map(|h| h.key.clone()).collect();
    assert!(keys.contains(&b"both".to_vec()), "matches both clauses");
    assert!(keys.contains(&b"term_only".to_vec()), "matches the bare term");
    assert!(keys.contains(&b"phrase_only".to_vec()), "matches the phrase");
    // `both` gets term + phrase contributions, so it outranks the others.
    assert_eq!(hits[0].key, b"both".to_vec(), "both clauses beat one: {keys:?}");
}

/// An unterminated quote is lenient — the remainder is read as bare
/// terms, so the query still answers instead of erroring.
#[test]
fn matches_query_unterminated_quote_is_lenient() {
    let s = phrase_seg();
    // No closing quote: `quick brown` becomes bare terms → OR query.
    let lenient = s.matches_query(b"\"quick brown", 10, None);
    let or = s.matches_scored(b"quick brown", 10, None);
    assert_eq!(lenient, or);
}

/// A phrase clause on a positionless segment verifies nothing, but a bare
/// term in the same query still matches — the OR keeps what it can prove.
#[test]
fn matches_query_phrase_clause_needs_positions_but_term_survives() {
    let mut s = TextSegment::new();
    s.apply(b"d1", Some(b"the quick brown fox"));
    s.apply(b"d2", Some(b"a slow green turtle named fox"));
    // "fox" OR phrase "quick brown": no positions, so the phrase proves
    // nothing, but "fox" still matches both docs.
    let hits = s.matches_query(b"fox \"quick brown\"", 10, None);
    assert_eq!(hits.len(), 2, "the bare term still matches: {hits:?}");
    // A pure phrase with no positions matches nothing.
    assert!(s.matches_query(b"\"quick brown\"", 10, None).is_empty());
}

/// Phrase scoring uses global stats when injected, so cross-shard phrase
/// hits are comparable the same way ordinary matches are.
#[test]
fn phrase_scores_against_injected_stats() {
    // Two shards, each with a length-symmetric doc containing the phrase
    // (equal dl and tf), so only the shared global stats decide the
    // score — the cross-shard comparability global BM25 exists for.
    let mut a = TextSegment::with_positions();
    let mut b = TextSegment::with_positions();
    a.apply(b"a1", Some(b"quick brown fox"));
    a.apply(b"a2", Some(b"only quick here"));
    b.apply(b"b1", Some(b"quick brown bear"));
    b.apply(b"b2", Some(b"nothing relevant"));
    let g = global_of(&[&a, &b], &[b"quick", b"brown"]);
    let ha = a.phrase_matches(b"quick brown", 10, Some(&g));
    let hb = b.phrase_matches(b"quick brown", 10, Some(&g));
    assert_eq!(ha.len(), 1);
    assert_eq!(hb.len(), 1);
    // Equal dl + tf + shared global stats → equal scores; if either shard
    // had scored with its own local df/avgdl they would diverge.
    assert!((ha[0].score - hb[0].score).abs() < 1e-9, "{} vs {}", ha[0].score, hb[0].score);
}

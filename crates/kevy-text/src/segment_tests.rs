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

// ---- highlight spans (step 5e.a) -----------------------------------------

/// A bare term highlights every occurrence, and each span slices back to
/// the source text (un-lowercased).
#[test]
fn highlight_marks_every_term_occurrence() {
    let text = "Rust is a systems language and rust is fast";
    let mut s = TextSegment::new();
    s.apply(b"d", Some(text.as_bytes()));
    let hl = s.highlight_spans(b"d", b"rust");
    assert_eq!(hl.len(), 1, "one field");
    let (fi, spans) = &hl[0];
    assert_eq!(*fi, 0);
    let hits: Vec<&str> = spans.iter().map(|(a, b)| &text[*a..*b]).collect();
    assert_eq!(hits, vec!["Rust", "rust"], "both occurrences, source-cased");
}

/// A phrase highlights only its adjacent, in-order occurrence — the words
/// scattered elsewhere are not the phrase.
#[test]
fn highlight_marks_only_the_adjacent_phrase() {
    let text = "the quick brown fox and a quick red brown thing";
    let mut s = TextSegment::new();
    s.apply(b"d", Some(text.as_bytes()));
    let hl = s.highlight_spans(b"d", b"\"quick brown\"");
    let (_, spans) = &hl[0];
    let hits: Vec<&str> = spans.iter().map(|(a, b)| &text[*a..*b]).collect();
    assert_eq!(hits, vec!["quick", "brown"], "only the adjacent pair, not the scattered words");
}

/// Highlighting reports spans per field, each indexing that field's text.
#[test]
fn highlight_is_per_field() {
    let (f0, f1) = ("rust title", "body about rust systems");
    let mut s = TextSegment::new();
    s.apply_fields(b"d", Some(&[(f0.as_bytes().to_vec(), 2.0), (f1.as_bytes().to_vec(), 1.0)]));
    let hl = s.highlight_spans(b"d", b"rust");
    assert_eq!(hl.len(), 2, "both fields match");
    assert_eq!(hl[0].0, 0);
    assert_eq!(&f0[hl[0].1[0].0..hl[0].1[0].1], "rust");
    assert_eq!(hl[1].0, 1);
    assert_eq!(&f1[hl[1].1[0].0..hl[1].1[0].1], "rust");
}

/// A CJK phrase highlights the bigrams covering the adjacent match.
#[test]
fn highlight_cjk_phrase() {
    let text = "全文检索引擎";
    let mut s = TextSegment::new();
    s.apply(b"d", Some(text.as_bytes()));
    // "检索" is one query bigram — a single-token phrase degrades to a
    // term, so it highlights the 检索 bigram's span.
    let hl = s.highlight_spans(b"d", "检索".as_bytes());
    let (_, spans) = &hl[0];
    let hits: Vec<&str> = spans.iter().map(|(a, b)| &text[*a..*b]).collect();
    assert_eq!(hits, vec!["检索"]);
}

/// An absent key or a query that matches nothing yields no spans.
#[test]
fn highlight_absent_or_no_match_is_empty() {
    let mut s = TextSegment::new();
    s.apply(b"d", Some(b"rust systems"));
    assert!(s.highlight_spans(b"nope", b"rust").is_empty(), "absent key");
    assert!(s.highlight_spans(b"d", b"python").is_empty(), "no match");
}

// ---- prefix query (step 6.a) ---------------------------------------------

fn prefix_seg() -> TextSegment {
    let mut s = TextSegment::new();
    s.apply(b"d1", Some(b"quick fox"));
    s.apply(b"d2", Some(b"quiet night"));
    s.apply(b"d3", Some(b"quality code"));
    s.apply(b"d4", Some(b"slow turtle"));
    s.apply(b"d5", Some(b"quick quiet quality")); // quick + quiet both qui-
    s
}

/// A prefix matches every indexed term that begins with it — the OR of
/// its expansion terms. `qui` reaches quick and quiet, not quality (qua-).
#[test]
fn prefix_expands_to_all_matching_terms() {
    let s = prefix_seg();
    let hits = s.matches_prefix(b"qui", 10, None);
    let keys: std::collections::HashSet<Vec<u8>> = hits.iter().map(|h| h.key.clone()).collect();
    // quick(d1,d5) quiet(d2,d5); d3 is quality (qua-), d4 slow — neither qui-.
    assert_eq!(
        keys,
        [b"d1".to_vec(), b"d2".to_vec(), b"d5".to_vec()].into_iter().collect(),
        "every qui- doc, and only those"
    );
    // d5 holds two expansion terms (quick, quiet) → outranks the single ones.
    assert_eq!(hits[0].key, b"d5".to_vec(), "the doc matching most expansions ranks first");
}

/// The prefix is matched case-insensitively against the lowercased token
/// form, and a narrower prefix expands to fewer terms.
#[test]
fn prefix_is_case_insensitive_and_narrows() {
    let s = prefix_seg();
    let upper = s.matches_prefix(b"QUI", 10, None);
    let lower = s.matches_prefix(b"qui", 10, None);
    assert_eq!(upper, lower, "prefix is ASCII case-insensitive");
    // "quie" only reaches quiet.
    let narrow = s.matches_prefix(b"quie", 10, None);
    let keys: Vec<Vec<u8>> = narrow.iter().map(|h| h.key.clone()).collect();
    assert_eq!(keys, vec![b"d2".to_vec(), b"d5".to_vec()], "only the quiet docs");
}

/// An empty prefix, a prefix that matches nothing, and a zero limit each
/// return no hits rather than everything.
#[test]
fn prefix_edge_cases_are_empty() {
    let s = prefix_seg();
    assert!(s.matches_prefix(b"", 10, None).is_empty(), "empty prefix is not 'match all'");
    assert!(s.matches_prefix(b"zzz", 10, None).is_empty(), "no term has this prefix");
    assert!(s.matches_prefix(b"qui", 0, None).is_empty(), "limit 0 is empty");
}

/// A prefix that is itself a full term still matches that term's docs
/// (the term begins with itself).
#[test]
fn prefix_equal_to_a_term_matches_it() {
    let s = prefix_seg();
    let hits = s.matches_prefix(b"quick", 10, None);
    let keys: std::collections::HashSet<Vec<u8>> = hits.iter().map(|h| h.key.clone()).collect();
    assert_eq!(keys, [b"d1".to_vec(), b"d5".to_vec()].into_iter().collect());
}

// ---- prefix in the query grammar (step 6.b) ------------------------------

/// `qui*` in the MATCH text routes through `matches_query` to the same
/// result as the `matches_prefix` primitive.
#[test]
fn query_grammar_prefix_equals_primitive() {
    let s = prefix_seg();
    let via_query = s.matches_query(b"qui*", 10, None);
    let via_prefix = s.matches_prefix(b"qui", 10, None);
    assert_eq!(via_query, via_prefix);
    assert!(!via_query.is_empty());
}

/// A query mixes bare terms, phrases and prefixes as an OR of clauses.
#[test]
fn query_grammar_mixes_prefix_with_terms() {
    let s = prefix_seg();
    // "slow" (d4) OR prefix "qui*" (d1, d2, d5).
    let hits = s.matches_query(b"slow qui*", 10, None);
    let keys: std::collections::HashSet<Vec<u8>> = hits.iter().map(|h| h.key.clone()).collect();
    assert_eq!(
        keys,
        [b"d1".to_vec(), b"d2".to_vec(), b"d4".to_vec(), b"d5".to_vec()].into_iter().collect(),
    );
    // A no-star, no-quote query is still the byte-identical term path.
    assert_eq!(s.matches_query(b"quick", 10, None), s.matches_scored(b"quick", 10, None));
}

/// HIGHLIGHT over a prefix query marks every token that begins with the
/// prefix.
#[test]
fn highlight_marks_prefix_matches() {
    let text = "quick quiet slowly";
    let mut s = TextSegment::new();
    s.apply(b"d", Some(text.as_bytes()));
    let hl = s.highlight_spans(b"d", b"qui*");
    let (_, spans) = &hl[0];
    let hits: Vec<&str> = spans.iter().map(|(a, b)| &text[*a..*b]).collect();
    assert_eq!(hits, vec!["quick", "quiet"], "both qui- tokens, not slowly");
}

// ---- typo tolerance (step 6.d) -------------------------------------------

fn typo_seg() -> TextSegment {
    let mut s = TextSegment::new();
    s.apply(b"d1", Some(b"quick brown fox"));
    s.apply(b"d2", Some(b"slow green turtle"));
    s
}

/// A budget of 1 reaches a one-edit misspelling; a budget of 0 does not.
#[test]
fn typo_budget_reaches_near_terms() {
    let s = typo_seg();
    // "quik" is one deletion from "quick".
    let fuzzy = s.matches_query_typo(b"quik", 10, None, 1);
    assert_eq!(fuzzy.len(), 1);
    assert_eq!(fuzzy[0].key, b"d1".to_vec());
    // Budget 0 is the exact query — "quik" is not indexed.
    assert!(s.matches_query_typo(b"quik", 10, None, 0).is_empty());
    // …and the sugar is the budget-0 form.
    assert_eq!(s.matches_query(b"quik", 10, None), s.matches_query_typo(b"quik", 10, None, 0));
}

/// The budget is a real bound: two edits need a budget of two.
#[test]
fn typo_budget_is_a_bound() {
    let s = typo_seg();
    // "qvik" is two edits from "quick" (substitute + delete).
    assert!(s.matches_query_typo(b"qvik", 10, None, 1).is_empty(), "over a budget of 1");
    let two = s.matches_query_typo(b"qvik", 10, None, 2);
    assert_eq!(two.len(), 1);
    assert_eq!(two[0].key, b"d1".to_vec());
}

/// An exact term still matches under a budget (distance 0 is included),
/// and a term far from everything matches nothing.
#[test]
fn typo_keeps_exact_and_rejects_far() {
    let s = typo_seg();
    let exact = s.matches_query_typo(b"quick", 10, None, 2);
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].key, b"d1".to_vec());
    assert!(s.matches_query_typo(b"zzzzz", 10, None, 2).is_empty());
}

/// Phrases and prefixes are not fuzzed — only bare terms are.
#[test]
fn typo_does_not_widen_phrases() {
    let mut s = TextSegment::with_positions();
    s.apply(b"d", Some(b"quick brown fox"));
    // A phrase with a misspelled token stays exact, so it matches nothing
    // even under a budget.
    assert!(s.matches_query_typo(b"\"quik brown\"", 10, None, 2).is_empty());
    // The correctly-spelled phrase still matches.
    assert_eq!(s.matches_query_typo(b"\"quick brown\"", 10, None, 2).len(), 1);
}

/// The df term set a cross-shard query aggregates includes the fuzzed
/// term's neighbours, so their global df is available at scoring time.
#[test]
fn typo_df_terms_include_neighbours() {
    let s = typo_seg();
    let exact = s.query_df_terms(b"quik");
    assert_eq!(exact, vec![b"quik".to_vec()], "budget 0 reports the term as written");
    let fuzzy = s.query_df_terms_typo(b"quik", 1);
    assert!(fuzzy.contains(&b"quick".to_vec()), "the neighbour is reported: {fuzzy:?}");
}

// ---- field-scoped queries (`IN <field…>`) -------------------------------

/// Two fields — a short title and a longer body — so a scoped query has
/// something to score differently from the whole document.
fn field_seg() -> TextSegment {
    let mut s = TextSegment::with_shape(SegmentShape { fields: 2, positions: true, ..Default::default() });
    s.apply_fields(
        b"a",
        Some(&[(b"rust engine".to_vec(), 1.0), (b"a long body about gardening".to_vec(), 1.0)]),
    );
    s.apply_fields(
        b"b",
        Some(&[
            (b"gardening weekly".to_vec(), 1.0),
            (b"this body mentions rust once among many other words here".to_vec(), 1.0),
        ]),
    );
    s
}

#[test]
fn scoping_to_every_field_equals_the_unscoped_query() {
    // The per-field channel is the *breakdown* of the merged posting, so
    // asking for all of the fields must reproduce the merged scoring
    // exactly — same frequencies, same length, same df. This is the
    // invariant that keeps the two paths from drifting apart.
    let s = field_seg();
    let all = &[0usize, 1][..];
    for q in ["rust", "gardening", "rust gardening", "body"] {
        let plain = s.matches_query(q.as_bytes(), 10, None);
        let scoped =
            s.matches_query_with(q.as_bytes(), 10, QueryOpts { fields: all, ..Default::default() });
        assert_eq!(plain.len(), scoped.len(), "hit count for {q:?}");
        for (p, sc) in plain.iter().zip(&scoped) {
            assert_eq!(p.key, sc.key, "ranking for {q:?}");
            assert!((p.score - sc.score).abs() < 1e-9, "score for {q:?}: {p:?} vs {sc:?}");
        }
    }
}

#[test]
fn scoping_restricts_matches_to_the_named_field() {
    let s = field_seg();
    // Unscoped, both documents mention rust.
    assert_eq!(s.matches_query(b"rust", 10, None).len(), 2);
    // Only "a" has it in the title.
    let title =
        s.matches_query_with(b"rust", 10, QueryOpts { fields: &[0], ..Default::default() });
    assert_eq!(title.len(), 1, "one title mentions rust");
    assert_eq!(title[0].key, b"a".to_vec());
    // Only "b" has it in the body.
    let body = s.matches_query_with(b"rust", 10, QueryOpts { fields: &[1], ..Default::default() });
    assert_eq!(body.len(), 1);
    assert_eq!(body[0].key, b"b".to_vec());
}

#[test]
fn scoped_score_normalises_by_the_field_not_the_document() {
    // "gardening" is a's whole-body topic and b's title. Scoped to the
    // title, b is scored against a two-token field, not against its long
    // body — the dilution a merged length normalisation would apply.
    let s = field_seg();
    let title =
        s.matches_query_with(b"gardening", 10, QueryOpts { fields: &[0], ..Default::default() });
    assert_eq!(title.len(), 1);
    assert_eq!(title[0].key, b"b".to_vec());

    // The same hit, scored over the whole document, is worth less: same
    // frequency, far more length to dilute it.
    let whole = s.matches_query(b"gardening", 10, None);
    let whole_b = whole.iter().find(|h| h.key == b"b".to_vec()).expect("b matches");
    assert!(
        title[0].score > whole_b.score,
        "field-scoped {} should beat whole-document {}",
        title[0].score,
        whole_b.score
    );
}

#[test]
fn scoped_document_frequency_counts_a_document_once() {
    let mut s = TextSegment::with_shape(SegmentShape { fields: 2, ..Default::default() });
    // "rust" in both fields of one document, one field of the other.
    s.apply_fields(b"a", Some(&[(b"rust".to_vec(), 1.0), (b"rust again".to_vec(), 1.0)]));
    s.apply_fields(b"b", Some(&[(b"other".to_vec(), 1.0), (b"rust".to_vec(), 1.0)]));

    let both = s.query_df_in(b"rust", QueryOpts { fields: &[0, 1], ..Default::default() });
    assert_eq!(both, vec![(b"rust".to_vec(), 2)], "a is one document, not two");
    let title = s.query_df_in(b"rust", QueryOpts { fields: &[0], ..Default::default() });
    assert_eq!(title, vec![(b"rust".to_vec(), 1)], "only a has it in field 0");
}

#[test]
fn scoped_phrase_must_lie_inside_one_wanted_field() {
    let mut s = TextSegment::with_shape(SegmentShape { fields: 2, positions: true, ..Default::default() });
    s.apply_fields(
        b"a",
        Some(&[(b"quick brown".to_vec(), 1.0), (b"nothing to see".to_vec(), 1.0)]),
    );
    // "brown fox" only reads as adjacent across the field boundary.
    s.apply_fields(b"b", Some(&[(b"the brown".to_vec(), 1.0), (b"fox sleeps".to_vec(), 1.0)]));

    let q = br#""quick brown""#;
    assert_eq!(s.matches_query(q, 10, None).len(), 1, "a has the phrase");
    let scoped = s.matches_query_with(q, 10, QueryOpts { fields: &[0], ..Default::default() });
    assert_eq!(scoped.len(), 1, "and it is in the title");
    assert!(
        s.matches_query_with(q, 10, QueryOpts { fields: &[1], ..Default::default() }).is_empty(),
        "not in the body"
    );

    let across = br#""brown fox""#;
    assert_eq!(s.matches_query(across, 10, None).len(), 1, "adjacent when concatenated");
    for f in [0usize, 1] {
        assert!(
            s.matches_query_with(across, 10, QueryOpts { fields: &[f], ..Default::default() })
                .is_empty(),
            "a phrase straddling the field boundary is in neither field ({f})"
        );
    }
}

#[test]
fn scoping_a_single_field_segment() {
    // One field keeps no per-field channel: scoping to it IS the
    // unscoped query, and scoping to a position it does not have matches
    // nothing rather than silently widening.
    let s = seg();
    assert_eq!(s.field_arity(), 1);
    let plain = s.matches_query(b"rust", 10, None);
    let scoped =
        s.matches_query_with(b"rust", 10, QueryOpts { fields: &[0], ..Default::default() });
    assert_eq!(plain, scoped);
    assert!(
        s.matches_query_with(b"rust", 10, QueryOpts { fields: &[1], ..Default::default() })
            .is_empty(),
        "no second field to scope to"
    );
}

#[test]
fn reindexing_keeps_the_field_channel_exact() {
    let mut s = field_seg();
    // Move "rust" out of a's title and into its body.
    s.apply_fields(
        b"a",
        Some(&[(b"garden engine".to_vec(), 1.0), (b"now the body says rust".to_vec(), 1.0)]),
    );
    let title =
        s.matches_query_with(b"rust", 10, QueryOpts { fields: &[0], ..Default::default() });
    assert!(title.is_empty(), "no title mentions rust any more");
    let mut body: Vec<Vec<u8>> = s
        .matches_query_with(b"rust", 10, QueryOpts { fields: &[1], ..Default::default() })
        .into_iter()
        .map(|h| h.key)
        .collect();
    body.sort();
    assert_eq!(body, vec![b"a".to_vec(), b"b".to_vec()], "both bodies do");

    // Removing the row empties both the channel and the corpus totals.
    s.apply_fields(b"a", None);
    s.apply_fields(b"b", None);
    assert_eq!(s.total_len_in(&[0, 1]), 0, "field totals drop with the documents");
    assert_eq!(s.total_len(), 0);
}

#[test]
fn scoped_typo_and_prefix_stay_inside_the_field() {
    let s = field_seg();
    let opts = |f: &'static [usize], typo: u32| QueryOpts { fields: f, typo, ..Default::default() };
    // "rus*" expands to rust, which is in a's title only.
    let pfx = s.matches_query_with(b"rus*", 10, opts(&[0], 0));
    assert_eq!(pfx.len(), 1);
    assert_eq!(pfx[0].key, b"a".to_vec());
    // One edit from "rusty" reaches rust — again title-only.
    let typo = s.matches_query_with(b"rusty", 10, opts(&[0], 1));
    assert_eq!(typo.len(), 1);
    assert_eq!(typo[0].key, b"a".to_vec());
    assert_eq!(s.matches_query_with(b"rusty", 10, opts(&[1], 1)).len(), 1, "b's body has rust");
}

// ---- stored values and FILTER -------------------------------------------

/// Ten documents that all match "rust", each with a stored `price`, so a
/// predicate has something to reject and the ranking has something to
/// lose if it is applied in the wrong order.
fn value_seg() -> TextSegment {
    let mut s = TextSegment::with_shape(SegmentShape { values: 1, ..Default::default() });
    for i in 0..10u32 {
        // Repeating the term makes the low-priced docs rank *worst*, so a
        // filter that keeps only cheap ones must reach past the leaders.
        let body = format!("rust {}", "rust ".repeat((10 - i) as usize));
        // Price falls with the term count, so the CHEAP documents are the
        // badly-ranked ones — the arrangement that makes "filter after
        // the top-K" visibly wrong instead of accidentally right.
        let price = format!("{}", (10 - i) * 10);
        s.apply_doc(
            format!("d{i}").as_bytes(),
            Some(&[(body.into_bytes(), 1.0)]),
            &[Some(price.as_bytes())],
        );
    }
    s
}

#[test]
fn a_predicate_reaches_past_the_unfiltered_leaders() {
    // The five cheapest documents are the five WORST scorers, so a
    // filter applied after a top-5 would return nothing at all. Applied
    // before, it returns exactly them. This is the ordering bug the
    // clause exists to not have.
    let s = value_seg();
    let cheap = |v: &[u8]| std::str::from_utf8(v).unwrap().parse::<u32>().unwrap() < 55;
    let f = [Filter { field: 0, test: &cheap }];
    let hits = s.matches_query_with(b"rust", 5, QueryOpts { filter: &f, ..Default::default() });
    let mut keys: Vec<Vec<u8>> = hits.into_iter().map(|h| h.key).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![b"d5".to_vec(), b"d6".to_vec(), b"d7".to_vec(), b"d8".to_vec(), b"d9".to_vec()],
        "the qualifying documents are found even though every one ranks below the top 5"
    );
    // Unfiltered, the top 5 are exactly the five the predicate rejects —
    // so filtering a top-5 after the fact would have returned nothing.
    let plain = s.matches_query(b"rust", 5, None);
    assert_eq!(plain[0].key, b"d0".to_vec(), "d0 repeats 'rust' most");
    let top: Vec<Vec<u8>> = plain.into_iter().map(|h| h.key).collect();
    assert_eq!(
        top,
        vec![b"d0".to_vec(), b"d1".to_vec(), b"d2".to_vec(), b"d3".to_vec(), b"d4".to_vec()]
    );
}

#[test]
fn predicates_are_anded_and_absent_never_passes() {
    let s = value_seg();
    let ge = |v: &[u8]| std::str::from_utf8(v).unwrap().parse::<u32>().unwrap() >= 30;
    let lt = |v: &[u8]| std::str::from_utf8(v).unwrap().parse::<u32>().unwrap() < 60;
    let both = [Filter { field: 0, test: &ge }, Filter { field: 0, test: &lt }];
    let hits = s.matches_query_with(b"rust", 10, QueryOpts { filter: &both, ..Default::default() });
    let mut keys: Vec<Vec<u8>> = hits.into_iter().map(|h| h.key).collect();
    keys.sort();
    assert_eq!(keys, vec![b"d5".to_vec(), b"d6".to_vec(), b"d7".to_vec()], "30 <= price < 60");

    // A field the segment does not store, and a document with no value,
    // both fail closed rather than passing.
    let any = |_: &[u8]| true;
    let missing = [Filter { field: 7, test: &any }];
    assert!(
        s.matches_query_with(b"rust", 10, QueryOpts { filter: &missing, ..Default::default() })
            .is_empty(),
        "an undeclared value field passes nobody"
    );
    let plain = seg();
    assert!(
        plain
            .matches_query_with(b"rust", 10, QueryOpts { filter: &missing, ..Default::default() })
            .is_empty(),
        "a segment storing no values passes nobody"
    );
}

#[test]
fn stored_values_follow_the_document() {
    let mut s = value_seg();
    let is5 = |v: &[u8]| v == b"5";
    let f = [Filter { field: 0, test: &is5 }];
    let q = |s: &TextSegment| {
        s.matches_query_with(b"rust", 10, QueryOpts { filter: &f, ..Default::default() }).len()
    };
    assert_eq!(q(&s), 0, "nothing is priced 5 yet");
    s.apply_doc(b"d3", Some(&[(b"rust".to_vec(), 1.0)]), &[Some(b"5")]);
    assert_eq!(q(&s), 1, "a re-index updates the stored value");
    s.apply_doc(b"d3", None, &[]);
    assert_eq!(q(&s), 0, "and a removal takes it away");

    // The freed id is handed to a new document, which must not inherit it.
    s.apply_doc(b"fresh", Some(&[(b"rust".to_vec(), 1.0)]), &[]);
    assert_eq!(q(&s), 0, "a reused id slot carries no stale value");
}

#[test]
fn the_memory_formula_counts_stored_values() {
    let bare = TextSegment::new().stats().approx_bytes;
    let s = value_seg();
    let with_values = s.stats().approx_bytes;
    let mut without = TextSegment::new();
    for i in 0..10u32 {
        let body = format!("rust {}", "rust ".repeat((10 - i) as usize));
        without.apply(format!("d{i}").as_bytes(), Some(body.as_bytes()));
    }
    assert!(with_values > without.stats().approx_bytes, "the value column is accounted for");
    assert!(with_values > bare);
}

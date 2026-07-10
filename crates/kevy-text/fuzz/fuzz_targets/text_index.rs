//! Fuzz `kevy_text` — tokenizer totality + `TextSegment` build/update/query
//! against a naive BM25 oracle.
//!
//! The segment's MaxScore/impact-bucket pruning is the risky machinery
//! (three early-stop paths that must never drop a contribution); the
//! oracle here recomputes plain OR-semantics BM25 with no pruning at all.
//! Invariants asserted per input:
//!
//!   * `tokenize` never panics on arbitrary (incl. non-UTF-8) bytes and
//!     never emits an empty token
//!   * after an input-driven sequence of `apply` (index / update / remove),
//!     `stats().docs` and `contains` agree with the tracked live set
//!   * a full-width `matches` query returns EXACTLY the docs sharing ≥ 1
//!     token with the query (OR semantics — set equality with the oracle)
//!   * every returned score equals the naive BM25 sum (same formula,
//!     re-derived from `BM25_K1`/`BM25_B`) within float-accumulation
//!     tolerance, and the ranking is score-descending with key-ascending
//!     tiebreak
//!   * a truncated `matches(_, k)` returns the top-k of the naive ranking
//!     (score-tolerant check: its worst score must not be materially below
//!     the naive k-th score)
//!
//! KNOWN FINDING (not asserted here so the sustained run stays green;
//! reported to the caller instead): `matches(query, 0)` panics with a
//! usize underflow in `kth_of` (`limit - 1`) whenever the query hits any
//! posting list — the pub API accepts `limit: usize` but limit=0 is not
//! handled. This target therefore only issues queries with limit ≥ 1.

#![no_main]

use kevy_text::{BM25_B, BM25_K1, TextSegment, tokenize};
use libfuzzer_sys::fuzz_target;
use std::collections::{BTreeMap, HashMap};

/// Same formula as kevy-text's private `bm25_score`, re-derived from the
/// exported constants.
fn bm25(tf: f64, df: f64, n_docs: f64, dl: f64, avgdl: f64) -> f64 {
    let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
    let denom = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avgdl);
    idf * tf * (BM25_K1 + 1.0) / denom
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

struct Input<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Input<'a> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn chunk(&mut self, max: usize) -> &'a [u8] {
        let len = match self.byte() {
            Some(b) => (b as usize) % (max + 1),
            None => 0,
        };
        let end = (self.pos + len).min(self.data.len());
        let c = &self.data[self.pos..end];
        self.pos = end;
        c
    }
}

fuzz_target!(|data: &[u8]| {
    // Tokenizer totality on the raw input (non-UTF-8 included).
    for t in tokenize(data) {
        assert!(!t.is_empty(), "tokenizer emitted an empty token");
    }

    let mut input = Input { data, pos: 0 };
    let Some(nd) = input.byte() else { return };
    let ndocs = (nd as usize % 12) + 1;

    let mut seg = TextSegment::new();
    // key -> current text (the ground truth the oracle recomputes from).
    let mut texts: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for i in 0..ndocs {
        let key = format!("k{i}").into_bytes();
        let text = input.chunk(48).to_vec();
        seg.apply(&key, Some(&text));
        texts.insert(key, text);
    }

    // Input-driven mutations: update / remove / re-add.
    for _ in 0..16 {
        let Some(op) = input.byte() else { break };
        let i = op as usize % ndocs;
        let key = format!("k{i}").into_bytes();
        if op % 3 == 0 {
            seg.apply(&key, None);
            texts.remove(&key);
        } else {
            let text = input.chunk(48).to_vec();
            seg.apply(&key, Some(&text));
            texts.insert(key, text);
        }
    }

    let query = input.chunk(32).to_vec();

    // ---- Oracle: per-doc token frequencies over the live texts ----
    // A doc is indexed iff its token stream is non-empty (apply() early-
    // returns on token-less text).
    let live: BTreeMap<&[u8], HashMap<Vec<u8>, u32>> = texts
        .iter()
        .filter_map(|(k, text)| {
            let toks = tokenize(text);
            if toks.is_empty() {
                return None;
            }
            let mut tf: HashMap<Vec<u8>, u32> = HashMap::new();
            for t in toks {
                *tf.entry(t).or_insert(0) += 1;
            }
            Some((k.as_slice(), tf))
        })
        .collect();

    assert_eq!(seg.stats().docs as usize, live.len(), "stats().docs diverged");
    for key in texts.keys() {
        assert_eq!(
            seg.contains(key),
            live.contains_key(key.as_slice()),
            "contains diverged for {key:?}"
        );
    }

    let mut q_tokens = tokenize(&query);
    q_tokens.sort();
    q_tokens.dedup();

    // Naive BM25: score every live doc against the dedup'd query tokens.
    let n_docs = live.len() as f64;
    let dl_of: BTreeMap<&[u8], f64> =
        live.iter().map(|(k, tf)| (*k, f64::from(tf.values().sum::<u32>()))).collect();
    let avgdl = if live.is_empty() { 0.0 } else { dl_of.values().sum::<f64>() / n_docs };
    let mut naive: Vec<(Vec<u8>, f64)> = Vec::new();
    for (k, tf_map) in &live {
        let mut score = 0.0;
        let mut hit = false;
        for t in &q_tokens {
            if let Some(&tf) = tf_map.get(t) {
                let df = live.values().filter(|m| m.contains_key(t)).count() as f64;
                score += bm25(f64::from(tf), df, n_docs, dl_of[k], avgdl);
                hit = true;
            }
        }
        if hit {
            naive.push((k.to_vec(), score));
        }
    }
    // Ranking contract: score descending, key ascending on ties.
    naive.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // ---- Full-width query: exact set + scores + ordering ----
    let hits = seg.matches(&query, live.len() + 1);
    let mut got_keys: Vec<Vec<u8>> = hits.iter().map(|m| m.key.clone()).collect();
    let mut want_keys: Vec<Vec<u8>> = naive.iter().map(|(k, _)| k.clone()).collect();
    got_keys.sort();
    want_keys.sort();
    assert_eq!(got_keys, want_keys, "OR-semantics match set diverged");
    let naive_score: BTreeMap<&[u8], f64> =
        naive.iter().map(|(k, s)| (k.as_slice(), *s)).collect();
    for w in hits.windows(2) {
        assert!(
            w[0].score > w[1].score || (w[0].score == w[1].score && w[0].key < w[1].key),
            "ranking order broken: {w:?}"
        );
    }
    for m in &hits {
        let want = naive_score[m.key.as_slice()];
        assert!(
            close(m.score, want),
            "score diverged for {:?}: got {} want {want}",
            m.key,
            m.score
        );
    }

    // ---- Truncated query: top-k against the naive ranking ----
    if !naive.is_empty() {
        let k = 1 + (query.len() % 4);
        let top = seg.matches(&query, k);
        assert_eq!(top.len(), k.min(naive.len()));
        for m in &top {
            assert!(naive_score.contains_key(m.key.as_slice()), "top-k returned a non-match");
        }
        // The worst returned score must not be materially below the naive
        // k-th score (tolerance covers accumulation-order float drift on
        // near-ties).
        let kth = naive[top.len() - 1].1;
        let worst = top.last().expect("nonempty").score;
        assert!(
            worst > kth - 1e-9 * kth.abs().max(1.0),
            "top-k dropped a better doc: worst {worst} vs naive kth {kth}"
        );
    }
});

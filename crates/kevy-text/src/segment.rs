//! [`TextSegment`] — one shard's inverted slice of one text index
//! (index-follows-key, same discipline as kevy-index's `Segment`).
//! Maintained synchronously with writes; queried with BM25 ranking
//! over shard-local statistics (RFC D2: per-shard df/avgdl — global
//! statistics would need cross-shard write coordination).

use std::collections::HashMap;

use crate::bm25::bm25_score;
use crate::token::tokenize;

/// One ranked hit.
#[derive(Debug, Clone, PartialEq)]
pub struct TextMatch {
    /// Row key.
    pub key: Vec<u8>,
    /// Shard-local BM25 score.
    pub score: f64,
}

/// Sizing counters (memory formula + IDX.LIST).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextStats {
    /// Indexed documents.
    pub docs: u64,
    /// Distinct tokens.
    pub tokens: u64,
    /// Total postings.
    pub postings: u64,
    /// Approximate heap bytes (RFC D4 formula's measured side).
    pub approx_bytes: u64,
}

/// One scoring candidate list: (postings, df, MaxScore upper bound).
type ScoredList<'s> = (&'s Buckets, f64, f64);

/// One token's postings, IMPACT-BUCKETED two ways (v3.5): keys
/// grouped by tf (buckets tf-DESCENDING), and WITHIN a bucket ordered
/// by dl ascending. BM25 is monotone ↑tf / ↓dl, so a single-list walk
/// goes bucket-by-bucket high-tf-first and INSIDE each bucket stops
/// the moment score(tf, dl) can't reach the kth floor — exact top-K
/// visits ~k·buckets postings instead of the whole list (the
/// single-common-term shape measured 1.5ms at 66k postings under the
/// tf-only stop; dl-ordering makes the within-bucket cut).
/// Where one posting lives inside its list: which tf bucket, which
/// dl band, which slot of the band's ids Vec. Kept in a LIST-level
/// map so probe and swap-remove are O(1) without per-band maps.
#[derive(Debug, Clone, Copy)]
struct Loc {
    tf: u32,
    band: u8,
    slot: u32,
}

/// dl → log2 band (band 0 = dl 1, band b covers 2^b..2^(b+1)-1,
/// capped at 15). The band's LOWER edge is the conservative dl for
/// bound tests: real scores inside the band can only be lower.
fn band_of(dl: u32) -> u8 {
    (31 - dl.max(1).leading_zeros()).min(15) as u8
}

const BAND_MIN_DL: [u32; 16] = [
    1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768,
];

/// One tf bucket: SPARSE dl bands (band asc), each a dense id Vec —
/// ordered walks are sequential 4B scans; per-id EXACT dl comes from
/// the segment's id_dl table (bands only set the CUT granularity).
type Bands = Vec<(u8, Vec<u32>)>;

/// One token's postings. Zipf text is mostly hapax legomena — a
/// unique id / email / doc number appears in exactly one row — so
/// the one-posting case stays INLINE (zero heap): the full structure
/// only materializes from the second posting on. (textgate caught
/// the eager shape at +2GiB RSS over 1M singleton tokens.)
#[derive(Debug)]
pub enum Buckets {
    One { id: u32, tf: u32, dl: u32 },
    Many(Box<ManyBuckets>),
}

#[derive(Debug, Default)]
pub struct ManyBuckets {
    /// (tf, bands) — sorted tf-descending; ≤ max_tf entries.
    buckets: Vec<(u32, Bands)>,
    /// id → location, for O(1) probe / remove across the whole list.
    index: HashMap<u32, Loc>,
}

impl Default for Buckets {
    fn default() -> Self {
        Buckets::Many(Box::default())
    }
}

impl ManyBuckets {
    fn insert(&mut self, tf: u32, dl: u32, id: u32) {
        let pos = self.buckets.iter().position(|(t, _)| *t <= tf);
        let slot = match pos {
            Some(i) if self.buckets[i].0 == tf => &mut self.buckets[i].1,
            Some(i) => {
                self.buckets.insert(i, (tf, Bands::new()));
                &mut self.buckets[i].1
            }
            None => {
                self.buckets.push((tf, Bands::new()));
                &mut self.buckets.last_mut().expect("just pushed").1
            }
        };
        let band = band_of(dl);
        let bi = match slot.binary_search_by_key(&band, |(b, _)| *b) {
            Ok(i) => i,
            Err(i) => {
                slot.insert(i, (band, Vec::new()));
                i
            }
        };
        let v = &mut slot[bi].1;
        self.index.insert(id, Loc { tf, band, slot: v.len() as u32 });
        v.push(id);
    }

    fn remove(&mut self, id: u32) {
        let Some(loc) = self.index.remove(&id) else { return };
        if let Some(i) = self.buckets.iter().position(|(t, _)| *t == loc.tf)
            && let Ok(bi) = self.buckets[i].1.binary_search_by_key(&loc.band, |(b, _)| *b)
        {
            let v = &mut self.buckets[i].1[bi].1;
            let si = loc.slot as usize;
            v.swap_remove(si);
            if let Some(&moved) = v.get(si) {
                self.index.get_mut(&moved).expect("indexed posting").slot = loc.slot;
            }
            if v.is_empty() {
                self.buckets[i].1.remove(bi);
            }
            if self.buckets[i].1.is_empty() {
                self.buckets.remove(i);
            }
        }
    }
}

impl Buckets {
    fn new_one(tf: u32, dl: u32, id: u32) -> Self {
        Buckets::One { id, tf, dl }
    }

    fn insert(&mut self, tf: u32, dl: u32, id: u32) {
        match self {
            Buckets::One { id: id0, tf: tf0, dl: dl0 } => {
                let (id0, tf0, dl0) = (*id0, *tf0, *dl0);
                let mut many = ManyBuckets::default();
                many.insert(tf0, dl0, id0);
                many.insert(tf, dl, id);
                *self = Buckets::Many(Box::new(many));
            }
            Buckets::Many(m) => m.insert(tf, dl, id),
        }
    }

    fn remove(&mut self, _tf: u32, _dl: u32, id: u32) {
        match self {
            Buckets::One { id: id0, .. } => {
                if *id0 == id {
                    // caller drops empty lists via is_empty()
                    *self = Buckets::Many(Box::default());
                }
            }
            Buckets::Many(m) => m.remove(id),
        }
    }

    fn len(&self) -> usize {
        match self {
            Buckets::One { .. } => 1,
            Buckets::Many(m) => m.index.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// O(1): buckets are tf-descending.
    fn max_tf(&self) -> u32 {
        match self {
            Buckets::One { tf, .. } => *tf,
            Buckets::Many(m) => m.buckets.first().map_or(1, |(t, _)| *t),
        }
    }

    /// O(1) membership probe over the whole list.
    fn get(&self, id: u32) -> Option<u32> {
        match self {
            Buckets::One { id: id0, tf, .. } => (*id0 == id).then_some(*tf),
            Buckets::Many(m) => m.index.get(&id).map(|l| l.tf),
        }
    }

    /// Per-query view: tf groups descending, each with its bands
    /// ascending. The One case borrows its single posting as a
    /// 1-slice; only the tiny outer Vec allocates (per query, per
    /// list).
    fn tf_groups(&self) -> Vec<(u32, BandsView<'_>)> {
        match self {
            Buckets::One { id, tf, dl } => {
                vec![(*tf, BandsView::One(band_of(*dl), std::slice::from_ref(id)))]
            }
            Buckets::Many(m) => m
                .buckets
                .iter()
                .map(|(t, bands)| (*t, BandsView::Slice(bands.as_slice())))
                .collect(),
        }
    }
}

/// Bands of one tf group, borrowed — see [`Buckets::tf_groups`].
enum BandsView<'a> {
    One(u8, &'a [u32]),
    Slice(&'a [(u8, Vec<u32>)]),
}

impl BandsView<'_> {
    /// (band, ids) pairs, band ascending.
    fn iter(&self) -> impl Iterator<Item = (u8, &[u32])> + '_ {
        let (one, slice) = match self {
            BandsView::One(b, ids) => (Some((*b, *ids)), [].as_slice()),
            BandsView::Slice(s) => (None, *s),
        };
        one.into_iter().chain(slice.iter().map(|(b, v)| (*b, v.as_slice())))
    }
}

/// One shard's inverted segment.
///
/// `postings` maps token → (key → tf) so a pruned list is PROBED per
/// accumulated candidate (O(candidates)) instead of walked
/// (O(postings)); `docs` keeps each row's original text so an update
/// removes exactly its own tokens (re-tokenize the old text) instead
/// of scanning every posting list.
#[derive(Debug, Default)]
pub struct TextSegment {
    postings: HashMap<Vec<u8>, Buckets>,
    /// key → (doc id, dl, original text).
    docs: HashMap<Vec<u8>, (u32, u32, Vec<u8>)>,
    /// id → key (None = freed slot, id on the free list).
    id_key: Vec<Option<Vec<u8>>>,
    /// id → dl (valid while id_key[id].is_some()).
    id_dl: Vec<u32>,
    free_ids: Vec<u32>,
    total_len: u64,
}

impl TextSegment {
    /// Empty segment.
    pub fn new() -> Self {
        Self::default()
    }

    /// (Re-)index one row's text (`None` = row removed / excluded).
    pub fn apply(&mut self, key: &[u8], text: Option<&[u8]>) {
        if let Some((old_id, old_len, old_text)) = self.docs.remove(key) {
            self.total_len -= u64::from(old_len);
            // Remove exactly this doc's tokens (re-derive tf from the
            // old text — O(doc), not O(index)).
            for (t, tf) in tf_of(&tokenize(&old_text)) {
                if let Some(list) = self.postings.get_mut(&t) {
                    list.remove(tf, old_len, old_id);
                    if list.is_empty() {
                        self.postings.remove(&t);
                    }
                }
            }
            self.id_key[old_id as usize] = None;
            self.free_ids.push(old_id);
        }
        let Some(text) = text else { return };
        let toks = tokenize(text);
        if toks.is_empty() {
            return;
        }
        let dl = toks.len() as u32;
        let id = match self.free_ids.pop() {
            Some(id) => {
                self.id_key[id as usize] = Some(key.to_vec());
                self.id_dl[id as usize] = dl;
                id
            }
            None => {
                self.id_key.push(Some(key.to_vec()));
                self.id_dl.push(dl);
                (self.id_key.len() - 1) as u32
            }
        };
        self.docs.insert(key.to_vec(), (id, dl, text.to_vec()));
        self.total_len += u64::from(dl);
        for (t, tf) in tf_of(&toks) {
            match self.postings.entry(t) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    e.get_mut().insert(tf, dl, id);
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(Buckets::new_one(tf, dl, id));
                }
            }
        }
    }

    /// BM25-ranked matches for `query` (tokenized with the same rules;
    /// OR semantics), best `limit` hits, score-descending.
    ///
    /// MaxScore pruning: query tokens process rarest-first; once the
    /// running top-`limit` threshold exceeds the summed upper bounds
    /// of the remaining (commoner) tokens, documents seen ONLY in
    /// those lists can no longer enter — their lists are then probed
    /// per accumulated doc instead of walked. Selection is a bounded
    /// heap over borrowed keys (no per-candidate allocation).
    pub fn matches(&self, query: &[u8], limit: usize) -> Vec<TextMatch> {
        let mut q_tokens = tokenize(query);
        q_tokens.sort();
        q_tokens.dedup();
        if q_tokens.is_empty() || self.docs.is_empty() {
            return Vec::new();
        }
        let n_docs = self.docs.len() as f64;
        let avgdl = self.total_len as f64 / n_docs;
        // (list, df, upper bound) — dl-independent bound: denom ≥
        // tf + k1(1-b), so score ≤ idf·tf(k1+1)/(tf + k1(1-b)).
        let mut lists: Vec<ScoredList<'_>> = Vec::new();
        for t in &q_tokens {
            let Some(list) = self.postings.get(t) else { continue };
            let df = list.len() as f64;
            let max_tf = f64::from(list.max_tf());
            lists.push((list, df, crate::bm25::bm25_upper(max_tf, df, n_docs)));
        }
        if lists.is_empty() {
            return Vec::new();
        }
        // rarest (highest upper bound) first
        lists.sort_by(|a, b| b.2.total_cmp(&a.2));
        let tail_ub: Vec<f64> = {
            // tail_ub[i] = Σ upper bounds of lists[i..]
            let mut acc = 0.0;
            let mut v: Vec<f64> = lists.iter().rev().map(|l| { acc += l.2; acc }).collect();
            v.reverse();
            v
        };
        let mut scores: HashMap<u32, f64> = HashMap::new();
        let mut kth_threshold = 0.0_f64;
        let mut walked = 0usize;
        for (i, (list, df, _ub)) in lists.iter().enumerate() {
            // Docs appearing only in the remaining lists can't reach
            // the current top-limit floor → stop WALKING; the loop
            // below PROBES these lists for already-seen docs.
            if i > 0 && scores.len() >= limit && tail_ub[i] < kth_threshold {
                break;
            }
            walked = i + 1;
            let groups = list.tf_groups();
            for (bi, (tf, bands)) in groups.iter().enumerate() {
                // Bucket-level early stop: buckets are tf-descending,
                // so once even the dl-free bound of THIS tf (plus
                // everything later lists could add) can't reach the
                // kth floor, no NEW doc from here on can enter. Docs
                // already accumulated still need this list's
                // contribution — the remaining buckets are PROBED for
                // them (a key has exactly one tf per token, so no
                // double count with earlier buckets).
                if scores.len() >= limit {
                    let bound = crate::bm25::bm25_upper(f64::from(*tf), *df, n_docs);
                    if bound + tail_ub[i + 1..].first().copied().unwrap_or(0.0)
                        < kth_of(&scores, limit)
                    {
                        let ids: Vec<u32> = scores.keys().copied().collect();
                        let walked_tfs: Vec<u32> =
                            groups[..bi].iter().map(|(t, _)| *t).collect();
                        for &id in &ids {
                            // One O(1) list-level probe replaces the
                            // per-bucket scan: contribute unless this
                            // id already got its tf from a WALKED
                            // bucket (tf is unique per (token, doc)).
                            if let Some(tf2) = list.get(id)
                                && !walked_tfs.contains(&tf2)
                            {
                                let dl_u = self.id_dl[id as usize];
                                *scores.get_mut(&id).expect("accumulated") += bm25_score(
                                    f64::from(tf2),
                                    *df,
                                    n_docs,
                                    f64::from(dl_u),
                                    avgdl,
                                );
                            }
                        }
                        break;
                    }
                }
                for (b, band) in bands.iter() {
                    if band.is_empty() {
                        continue;
                    }
                    // v3.5 single-list within-bucket cut: bands are
                    // dl-ASCENDING and BM25 falls as dl rises, so the
                    // band's LOWER dl edge bounds every score inside
                    // it from above. On a one-list query — each doc
                    // appears exactly ONCE in the whole list, no
                    // later contribution to lose — the first band
                    // whose bound can't beat the kth floor ends the
                    // bucket exactly. Scoring stays per-id exact via
                    // the id_dl table; bands only gate the cut.
                    let bound = bm25_score(
                        f64::from(*tf),
                        *df,
                        n_docs,
                        f64::from(BAND_MIN_DL[b as usize]),
                        avgdl,
                    );
                    if lists.len() == 1
                        && scores.len() >= limit
                        && bound < kth_of(&scores, limit)
                    {
                        break;
                    }
                    for &id in band {
                        let dl = f64::from(self.id_dl[id as usize]);
                        *scores.entry(id).or_insert(0.0) +=
                            bm25_score(f64::from(*tf), *df, n_docs, dl, avgdl);
                    }
                }
            }
            if scores.len() >= limit && i + 1 < lists.len() {
                kth_threshold = kth_of(&scores, limit);
            }
        }
        // Probe un-walked lists PER ACCUMULATED DOC — O(candidates)
        // hash gets, never a walk of the common list (walking here
        // was the measured 30ms p95: a pruned 500k-posting head list
        // still cost a full scan).
        if walked < lists.len() {
            let ids: Vec<u32> = scores.keys().copied().collect();
            for (list, df, _) in &lists[walked..] {
                for &id in &ids {
                    if let Some(tf) = list.get(id) {
                        let dl_u = self.id_dl[id as usize];
                        *scores.get_mut(&id).expect("accumulated") += bm25_score(
                            f64::from(tf),
                            *df,
                            n_docs,
                            f64::from(dl_u),
                            avgdl,
                        );
                    }
                }
            }
        }
        // Bounded selection: only the winners get cloned. Ids resolve
        // to keys here — the tiebreak (key ascending) is unchanged.
        let key_of = |id: u32| -> &[u8] {
            self.id_key[id as usize].as_deref().expect("live posting id")
        };
        let mut top: Vec<(f64, &[u8])> = Vec::with_capacity(limit + 1);
        for (id, score) in &scores {
            let cand = (*score, key_of(*id));
            if top.len() < limit {
                top.push(cand);
                if top.len() == limit {
                    top.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
                }
            } else if better(cand, top[limit - 1]) {
                let pos = top
                    .partition_point(|e| better(*e, cand));
                top.insert(pos, cand);
                top.pop();
            }
        }
        if top.len() < limit {
            top.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        }
        top.into_iter()
            .map(|(score, k)| TextMatch { key: k.to_vec(), score })
            .collect()
    }

    /// Live counters.
    pub fn stats(&self) -> TextStats {
        let postings: u64 = self.postings.values().map(|l| l.len() as u64).sum();
        // Hapax lists are INLINE (enum One) — no heap beyond their
        // postings-map slot; only Many lists pay the per-posting
        // band-vec + index costs.
        let many_postings: u64 = self
            .postings
            .values()
            .map(|l| match l {
                Buckets::One { .. } => 0,
                Buckets::Many(m) => m.index.len() as u64,
            })
            .sum();
        let token_bytes: u64 = self.postings.keys().map(|t| (t.len() + 48) as u64).sum();
        // docs table + the id→key/id→dl tables (key stored twice).
        let doc_bytes: u64 = self
            .docs
            .iter()
            .map(|(k, (_, _, text))| (2 * k.len() + text.len() + 110) as u64)
            .sum();
        TextStats {
            docs: self.docs.len() as u64,
            tokens: self.postings.len() as u64,
            postings,
            // per-Many-posting ≈ 4B band-vec slot + ~26B list-index
            // entry (doc-id postings + log2 dl bands, v3.5); hapax
            // lists are inline. Docs keep their original text
            // (update path re-derives tokens).
            approx_bytes: token_bytes + many_postings * 30 + doc_bytes,
        }
    }

    /// Verify hook: is `key` indexed here?
    pub fn contains(&self, key: &[u8]) -> bool {
        self.docs.contains_key(key)
    }
}

/// Aggregate token counts for one document's token stream.
fn tf_of(toks: &[Vec<u8>]) -> HashMap<Vec<u8>, u32> {
    let mut tf = HashMap::new();
    for t in toks {
        *tf.entry(t.clone()).or_insert(0) += 1;
    }
    tf
}

/// Strict "ranks ahead of" for (score, key) — higher score first,
/// key ascending as the tiebreak.
fn better(a: (f64, &[u8]), b: (f64, &[u8])) -> bool {
    a.0 > b.0 || (a.0 == b.0 && a.1 < b.1)
}

/// The `limit`-th best score currently accumulated (the MaxScore
/// entry floor). O(n) selection, called only between list walks.
fn kth_of(scores: &HashMap<u32, f64>, limit: usize) -> f64 {
    let mut v: Vec<f64> = scores.values().copied().collect();
    let idx = limit - 1;
    v.select_nth_unstable_by(idx, |a, b| b.total_cmp(a));
    v[idx]
}

#[cfg(test)]
mod tests {
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
}

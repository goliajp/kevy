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

/// One shard's inverted segment.
#[derive(Debug, Default)]
pub struct TextSegment {
    postings: HashMap<Vec<u8>, Vec<(Vec<u8>, u32)>>,
    doc_len: HashMap<Vec<u8>, u32>,
    total_len: u64,
}

impl TextSegment {
    /// Empty segment.
    pub fn new() -> Self {
        Self::default()
    }

    /// (Re-)index one row's text (`None` = row removed / excluded).
    pub fn apply(&mut self, key: &[u8], text: Option<&[u8]>) {
        if let Some(old_len) = self.doc_len.remove(key) {
            self.total_len -= u64::from(old_len);
            // Remove this key from every posting list it appears in.
            self.postings.retain(|_, list| {
                list.retain(|(k, _)| k != key);
                !list.is_empty()
            });
        }
        let Some(text) = text else { return };
        let toks = tokenize(text);
        if toks.is_empty() {
            return;
        }
        let mut tf: HashMap<Vec<u8>, u32> = HashMap::new();
        for t in &toks {
            *tf.entry(t.clone()).or_insert(0) += 1;
        }
        self.doc_len.insert(key.to_vec(), toks.len() as u32);
        self.total_len += toks.len() as u64;
        for (t, n) in tf {
            self.postings.entry(t).or_default().push((key.to_vec(), n));
        }
    }

    /// BM25-ranked matches for `query` (tokenized with the same rules;
    /// OR semantics), best `limit` hits, score-descending.
    pub fn matches(&self, query: &[u8], limit: usize) -> Vec<TextMatch> {
        let q_tokens = tokenize(query);
        if q_tokens.is_empty() || self.doc_len.is_empty() {
            return Vec::new();
        }
        let n_docs = self.doc_len.len() as f64;
        let avgdl = self.total_len as f64 / n_docs;
        let mut scores: HashMap<&[u8], f64> = HashMap::new();
        for t in &q_tokens {
            let Some(list) = self.postings.get(t) else { continue };
            let df = list.len() as f64;
            for (key, tf) in list {
                let dl = f64::from(self.doc_len.get(key.as_slice()).copied().unwrap_or(1));
                *scores.entry(key.as_slice()).or_insert(0.0) +=
                    bm25_score(f64::from(*tf), df, n_docs, dl, avgdl);
            }
        }
        let mut out: Vec<TextMatch> = scores
            .into_iter()
            .map(|(k, score)| TextMatch { key: k.to_vec(), score })
            .collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
        out.truncate(limit);
        out
    }

    /// Live counters.
    pub fn stats(&self) -> TextStats {
        let postings: u64 = self.postings.values().map(|l| l.len() as u64).sum();
        let token_bytes: u64 = self.postings.keys().map(|t| (t.len() + 48) as u64).sum();
        TextStats {
            docs: self.doc_len.len() as u64,
            tokens: self.postings.len() as u64,
            postings,
            approx_bytes: token_bytes + postings * 24 + self.doc_len.len() as u64 * 32,
        }
    }

    /// Verify hook: is `key` indexed here?
    pub fn contains(&self, key: &[u8]) -> bool {
        self.doc_len.contains_key(key)
    }
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
    fn limit_and_empty_query() {
        let s = seg();
        assert_eq!(s.matches(b"rust", 2).len(), 2);
        assert!(s.matches(b"", 10).is_empty());
        assert!(s.matches(b"!!!", 10).is_empty());
    }
}

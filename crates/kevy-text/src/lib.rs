//! kevy-text — dictionary-free full-text core:
//! script-aware tokenization (Latin words + CJK bigrams),
//! per-shard inverted segments maintained synchronously with writes,
//! BM25 ranking with shard-local statistics.
//!
//! ```
//! use kevy_text::TextSegment;
//!
//! let mut seg = TextSegment::new();
//! seg.apply(b"doc:1", Some("Rust storage engine".as_bytes()));
//! seg.apply(b"doc:2", Some("全文検索 in Rust".as_bytes()));
//! seg.apply(b"doc:3", Some(b"nothing relevant"));
//!
//! let hits = seg.matches(b"rust", 10);
//! let keys: Vec<&[u8]> = hits.iter().map(|m| m.key.as_slice()).collect();
//! assert_eq!(keys.len(), 2);
//! assert!(keys.contains(&&b"doc:1"[..]) && keys.contains(&&b"doc:2"[..]));
//!
//! // CJK needs no dictionary: bigrams find a two-character query.
//! assert_eq!(seg.matches("検索".as_bytes(), 10)[0].key, b"doc:2");
//! ```

#![warn(missing_docs)]

mod bm25;
mod buckets;
mod clauses;
pub mod cold;
mod docblobs;
mod docvalues;
mod edit;
mod fields;
mod positions;
mod segment;
mod token;

pub use bm25::{BM25_B, BM25_K1};
pub use segment::sorted_order;
pub use segment::{
    Bucket, CorpusStats, Distinct, Facet, FacetedMatches, Filter, QueryOpts, SegmentShape, Sort,
    SortOrder, TextMatch, TextSegment, TextStats,
};
pub use segment::{Clauses, parse_clauses};
pub use token::{KevyTokenizer, Tokenizer, tokenize, tokenize_spans};

// Send and Sync are part of the public contract: a change that loses
// either fails to compile here rather than in a caller.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<TextSegment>();
    send_sync::<TextMatch>();
    send_sync::<TextStats>();
    send_sync::<CorpusStats>();
    send_sync::<SegmentShape>();
    send_sync::<FacetedMatches>();
    send_sync::<SortOrder>();
    send_sync::<KevyTokenizer>();
    send_sync::<cold::ColdEntry>();
    send_sync::<cold::FrozenBucket>();
    send_sync::<cold::FwdRecord>();
};

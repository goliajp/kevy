//! kevy-text — dictionary-free full-text core:
//! script-aware tokenization (Latin words + CJK bigrams),
//! per-shard inverted segments maintained synchronously with writes,
//! BM25 ranking with shard-local statistics.

#![warn(missing_docs)]

mod bm25;
mod buckets;
mod positions;
mod segment;
mod token;

pub use bm25::{BM25_B, BM25_K1};
pub use segment::{CorpusStats, TextMatch, TextSegment, TextStats};
pub use token::{KevyTokenizer, Tokenizer, tokenize, tokenize_spans};

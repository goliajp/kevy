//! kevy-text — dictionary-free full-text core (RFC 2026-07-04,
//! LOCKED): script-aware tokenization (Latin words + CJK bigrams),
//! per-shard inverted segments maintained synchronously with writes,
//! BM25 ranking with shard-local statistics.

mod bm25;
mod segment;
mod token;

pub use bm25::{BM25_B, BM25_K1};
pub use segment::{TextMatch, TextSegment, TextStats};
pub use token::{KevyTokenizer, Tokenizer, tokenize};

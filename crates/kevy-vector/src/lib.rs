//! kevy-vector — ANN core (RFC 2026-07-04, LOCKED): HNSW graph with
//! cosine / L2 / inner-product distances, tombstone deletes filtered
//! at search time, bounded full rebuild.

#![warn(missing_docs)]

mod dist;
mod hnsw;

pub use dist::{Distance, parse_vector};
pub use hnsw::{Hnsw, HnswParams, VectorStats};

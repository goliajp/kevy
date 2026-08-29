//! kevy-vector — ANN core: HNSW graph with
//! cosine / L2 / inner-product distances, tombstone deletes filtered
//! at search time, bounded full rebuild.

#![warn(missing_docs)]

mod dist;
mod hnsw;
mod params;

pub use dist::{Distance, parse_vector};
pub use hnsw::Hnsw;
pub use params::{HnswParams, VectorStats};

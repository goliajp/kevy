//! kevy-vector — ANN core: HNSW graph with
//! cosine / L2 / inner-product distances, tombstone deletes filtered
//! at search time, bounded full rebuild.

#![warn(missing_docs)]

mod dist;
mod hnsw;
mod params;

/// Distance computations, counted — the cost axis a recall number has
/// to be plotted against.
///
/// `ef` is a knob, not a cost. Two graphs answering the same query at
/// the same `ef` can differ several-fold in how many distances they
/// evaluate, and every design question here — whether the neighbour
/// backfill earns its degree, whether tombstones are eating the beam,
/// whether a layout change paid — is a question about that number at a
/// fixed recall. Without it, "recall 0.77 at ef 100" cannot be compared
/// with anything, which is why the note recording exactly that in
/// `Hnsw::knn` was never followed up.
///
/// Behind a feature so a release build is byte-identical without it:
/// this sits inside `Distance::eval`, which is the hot loop.
#[cfg(feature = "count-distances")]
pub mod count {
    use core::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn bump() {
        N.fetch_add(1, Ordering::Relaxed);
    }

    /// Read the counter and reset it, so the next read is a fresh
    /// interval rather than a running total nobody differenced.
    pub fn take() -> u64 {
        N.swap(0, Ordering::Relaxed)
    }

    /// Read without resetting.
    #[must_use]
    pub fn peek() -> u64 {
        N.load(Ordering::Relaxed)
    }
}

pub use dist::{Distance, parse_vector};
pub use hnsw::Hnsw;
pub use params::{HnswParams, VectorStats};

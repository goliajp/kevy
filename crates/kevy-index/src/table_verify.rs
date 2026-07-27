//! Named verify-report types, shared by both faces.
//!
//! The first release shipped the embedded report as
//! `(Vec<(Vec<u8>, [u64; 6])>, [u64; 2])` — six unnamed counters per
//! index and two more for the spot check. The dogfood report's F10
//! ("two counters side by side with different time semantics and
//! nothing saying so") is the direct consequence: an anonymous array
//! has nowhere to write what a number means. These structs are where
//! that goes.

/// Per-index verification counters, one row of `TABLE.VERIFY`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexVerify {
    /// Compiled index name (`<table>.<column-or-orderpath>`).
    pub name: Vec<u8>,
    /// Entries currently held by the index.
    pub entries: u64,
    /// Approximate resident bytes of the index structure.
    pub approx_bytes: u64,
    /// Rows whose declared-typed column failed to coerce.
    ///
    /// **Lifetime counter in v4.0/v4.1-V1** (never resets, so a fixed
    /// problem keeps reading as one); recomputed fresh per call from
    /// v4.1-V4 — the semantics this name promises.
    pub coerce_failures: u64,
    /// Distinct sort keys held by more than one row. Non-zero means the
    /// sort is not a total order — a paged reader can skip or repeat
    /// rows at page boundaries. Add a bounded tie-break column.
    pub duplicates: u64,
    /// Entries whose held value disagrees with re-deriving from the row
    /// right now. Recomputed fresh on every call.
    pub drift: u64,
    /// Entries the drift recheck examined this call.
    pub checked: u64,
}

/// The whole `TABLE.VERIFY` answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableVerify {
    /// One row per compiled index of the table.
    pub per_index: Vec<IndexVerify>,
    /// Rows the bounded column-type spot check sampled this call.
    pub spot_rows: u64,
    /// Of those, rows holding a value that contradicts the declared
    /// column type.
    pub spot_type_mismatches: u64,
}

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

/// What `table_ensure` found (the boot verb — see the embedded docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableEnsure {
    /// The table did not exist; it was declared and its indexes built.
    Created,
    /// An identical declaration already exists; nothing was touched.
    Unchanged,
}

/// Name the first difference between an admitted spec and a proposed
/// one — the message `table_ensure` refuses with. Specific enough to
/// act on, short enough for a boot log.
#[must_use]
pub fn spec_diff(cur: &crate::TableSpec, new: &crate::TableSpec) -> String {
    let part = if cur.prefix != new.prefix {
        "PREFIX"
    } else if cur.pk != new.pk {
        "PK"
    } else if cur.columns != new.columns {
        "COLUMNS"
    } else if cur.indexes != new.indexes {
        "INDEXES"
    } else if cur.orderpaths != new.orderpaths {
        "ORDERPATHS"
    } else {
        "SPEC"
    };
    format!(
        "ERR table '{}' exists with a different spec ({part} differ); \
         TABLE.REPLACE rebuilds, TABLE.DROP + DECLARE is the manual form",
        String::from_utf8_lossy(&cur.name)
    )
}

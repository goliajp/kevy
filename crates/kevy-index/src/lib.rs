//! kevy-index — declarative secondary indexes over prefix domains.
//!
//! Pure logic crate: [`Catalog`] holds the index declarations and a
//! compiled prefix matcher; [`Segment`] holds one shard's slice of one
//! index (entries follow the indexed key's shard — index-follows-key).
//! The embedding runtime calls [`Segment::apply`] synchronously with
//! every write in the index's domain (derived-by-construction: the
//! index can never drift from the data, and [`Segment::verify_entry`]
//! makes that falsifiable), and serves queries by fanning
//! [`Segment::range`] / [`Segment::eq`] across shards and merging.
//!
//! No I/O, no threads, no runtime types — everything here is
//! deterministic and unit-testable in isolation.

#![warn(missing_docs)]

mod agg;
mod catalog;
mod catalog_sidecar;
mod composite;
mod rowvalues;
mod segment;
mod advise;
mod segment_claused;
mod table;
mod table_verify;
mod segcold;
mod table_sidecar;
mod table_wire;
mod value;
mod view;
mod view_sidecar;

pub use agg::{AggBy, AggSegment, AggStats, GroupStats, merge_group, sort_groups};
pub use catalog::{
    AnnSpec, Catalog, FieldSpec, IndexKind, IndexSpec, IndexState, RowInputs, ValType, ValueSpec,
};
pub use composite::{
    CompositeCol, MAX_COMPOSITE_COLS, MAX_STR_COMPONENT, WHERE_NOT_COMPOSITE, WhereClause,
    RowDerivation, composite_bounds, composite_encode, parse_where,
};
pub use segment::{Cursor, Segment, SegmentStats};
pub use advise::{ADVISE_CAP, AdviseEntry, AdviseLog, AdviseShape, advice_of};
pub use segment_claused::{
    ClausedPage, ColdEntryRow, FacetBucket, ScalarClauses, ScalarHit, claused_over, fold_facets,
    merge_claused, values_pass,
    scalar_sorted_order, sort_facets,
};
pub use table::{
    MAX_TABLES, OrderPath, TableCatalog, TableIndex, TableSpec, WindowSpec, compile_table,
    window_driver, window_for, window_text_for,
};
pub use segcold::{
    ColdBloom, WindowShape, decode_seg_key, decode_seg_values, encode_seg_values, seg_bounds,
    seg_key, value_order_bytes, window_bound, window_value_of,
};
pub use table_verify::{IndexVerify, TableEnsure, TableVerify, spec_diff};
pub use table_wire::{TABLE_DECLARE_USAGE, parse_table_declare};
pub use value::{IndexValue, ValueTest, order_key, coerce_bound, parse_literal_bound};
pub use view::{
    Leaf, MAX_TREE_DEPTH, MAX_TREE_LEAVES, MAX_VIEWS, MaterializedSet, Tree, ViewCatalog,
    ViewMode, ViewSpec, eval_tree, key_in_tree, key_in_tree_vals,
};

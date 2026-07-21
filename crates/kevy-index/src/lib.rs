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
mod segment;
mod value;
mod view;
mod view_sidecar;

pub use agg::{AggBy, AggSegment, AggStats, GroupStats, merge_group, sort_groups};
pub use catalog::{
    AnnSpec, Catalog, FieldSpec, IndexKind, IndexSpec, IndexState, RowInputs, ValType, ValueSpec, ValueTest,
};
pub use segment::{Cursor, Segment, SegmentStats};
pub use value::IndexValue;
pub use view::{
    Leaf, MAX_TREE_DEPTH, MAX_TREE_LEAVES, MAX_VIEWS, MaterializedSet, Tree, ViewCatalog,
    ViewMode, ViewSpec, eval_tree, key_in_tree, key_in_tree_vals,
};

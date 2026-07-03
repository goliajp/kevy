//! kevy-index — declarative secondary indexes over prefix domains
//! (RFC 2026-07-04, LOCKED).
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

mod catalog;
mod segment;
mod value;
mod view;

pub use catalog::{Catalog, IndexKind, IndexSpec, IndexState, ValType};
pub use segment::{Cursor, Segment, SegmentStats};
pub use value::IndexValue;
pub use view::{
    Leaf, MAX_TREE_DEPTH, MAX_TREE_LEAVES, MAX_VIEWS, MaterializedSet, Tree, ViewCatalog,
    ViewMode, ViewSpec, eval_tree, key_in_tree, key_in_tree_vals,
};

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
//!
//! ```
//! use kevy_index::{Catalog, IndexKind, IndexSpec, IndexValue, Segment, ValType};
//!
//! let age = IndexSpec::builder("age", "user:", IndexKind::Range, ValType::I64)
//!     .with_field("age")
//!     .build()?;
//! let mut catalog = Catalog::new();
//! catalog.create(age)?;
//! assert_eq!(catalog.matching(b"user:7").count(), 1, "the write hook finds the index");
//!
//! // one shard's slice, fed by the write path
//! let mut seg = Segment::new();
//! seg.apply(b"user:7", IndexValue::coerce(ValType::I64, b"41"));
//! seg.apply(b"user:9", IndexValue::coerce(ValType::I64, b"29"));
//! let (hits, next) = seg.range(&IndexValue::I64(30), &IndexValue::I64(50), None, 10);
//! assert_eq!(hits, vec![(b"user:7".to_vec(), IndexValue::I64(41))]);
//! assert!(next.is_none());
//! # Ok::<(), &'static str>(())
//! ```

#![warn(missing_docs)]

mod advise;
mod agg;
mod catalog;
mod catalog_sidecar;
mod composite;
mod describe;
mod describe_table;
mod describe_view;
mod partition;
#[cfg(test)]
mod partition_tests;
mod placement;
mod rowvalues;
mod segcold;
mod segment;
mod segment_claused;
mod segment_entry;
mod spec;
mod spec_builder;
mod spec_parts;
mod table;
mod table_catalog;
mod table_sidecar;
mod table_verify;
mod table_wire;
mod value;
mod view;
mod view_materialized;
mod view_sidecar;

pub use advise::{ADVISE_CAP, AUTODECLARE_AFTER, AdviseEntry, AdviseLog, AdviseShape, UsageCell};
pub use agg::{AggBy, AggRow, AggSegment, AggStats, GroupStats, sort_groups};
pub use catalog::{Catalog, IndexKind, IndexState, ValType};
pub use composite::{
    CompositeCol, MAX_COMPOSITE_COLS, MAX_STR_COMPONENT, RowDerivation, WHERE_NOT_COMPOSITE,
    WhereClause, composite_bounds, composite_encode, parse_where,
};
pub use describe::{
    Described, describe_index, describe_index_partitioned, describe_table,
    describe_table_partitioned, describe_view, index_declaration, index_declaration_partitioned,
    owner_of, table_declaration, table_declaration_partitioned, view_declaration,
};
pub use kevy_text::{SortOrder, sorted_order};
pub use partition::{Partitioning, partition_owner, splits_from_weighted};
pub use placement::PlacementTable;
pub use segcold::{
    ColdBloom, WindowAudit, WindowShape, decode_seg_key, decode_seg_values, encode_seg_values,
    seg_bounds, seg_key, window_bound,
};
pub use segment::{Cursor, Segment, SegmentStats};
pub use segment_claused::{
    ClausedPage, ColdEntryRow, FacetBucket, ScalarClauses, ScalarHit, claused_over, fold_facets,
    merge_claused, sort_facets, values_pass,
};
pub use spec::{IndexSpec, RowInputs};
pub use spec_builder::IndexSpecBuilder;
pub use spec_parts::{AnnSpec, FieldSpec, ValueSpec};
pub use table::{MAX_TABLES, OrderPath, TableCatalog, TableIndex, TableSpec, WindowSpec};
pub use table_verify::{IndexVerify, TableEnsure, TableVerify, spec_diff};
pub use table_wire::{
    GlobalPath, TABLE_DECLARE_USAGE, parse_table_declare, parse_table_declare_partitioned,
};
pub use value::{IndexValue, ValueTest, coerce_bound, order_key, parse_literal_bound};
pub use view::{
    Leaf, MAX_TREE_DEPTH, MAX_TREE_LEAVES, MAX_VIEWS, MaterializedSet, Membership, Tree,
    ViewCatalog, ViewMode, ViewSpec,
};

// Send and Sync are part of the public contract: a change that loses
// either fails to compile here rather than in a caller.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Catalog>();
    send_sync::<IndexSpec>();
    send_sync::<IndexSpecBuilder>();
    send_sync::<FieldSpec>();
    send_sync::<ValueSpec>();
    send_sync::<AnnSpec>();
    send_sync::<Segment>();
    send_sync::<Cursor>();
    send_sync::<SegmentStats>();
    send_sync::<AggSegment>();
    send_sync::<AggRow>();
    send_sync::<GroupStats>();
    send_sync::<AggStats>();
    send_sync::<AdviseLog>();
    send_sync::<AdviseEntry>();
    send_sync::<UsageCell>();
    send_sync::<TableSpec>();
    send_sync::<TableCatalog>();
    send_sync::<TableVerify>();
    send_sync::<GlobalPath>();
    send_sync::<ViewSpec>();
    send_sync::<ViewCatalog>();
    send_sync::<MaterializedSet>();
    send_sync::<Membership>();
    send_sync::<IndexValue>();
    send_sync::<ValueTest>();
    send_sync::<ScalarClauses<'static>>();
    send_sync::<ClausedPage>();
    send_sync::<WhereClause>();
    send_sync::<CompositeCol>();
    send_sync::<RowDerivation>();
    send_sync::<Described>();
    send_sync::<Partitioning>();
    send_sync::<PlacementTable>();
    send_sync::<ColdBloom>();
    send_sync::<WindowAudit>();
};

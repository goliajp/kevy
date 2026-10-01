//! Redis-compatible Streams storage. Each stream is an append-only log
//! of (ID, field-value-list) entries keyed by a monotonically increasing
//! `<ms>-<seq>` ID. The entries live in a `BTreeMap<StreamId, _>` so
//! range queries are O(log n + k) and the iterator natural order is the
//! ID order (ascending).
//!
//! Sprint A scope: bare stream (no consumer groups). The `StreamData`
//! type carries a `groups` slot reserved for sprint B; this file only
//! implements the entry-side ops.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use alloc::collections::BTreeMap;
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
#[cfg(not(any(
    feature = "external-clock",
    all(target_arch = "wasm32", target_os = "unknown")
)))]
use std::time::{SystemTime, UNIX_EPOCH};

use kevy_map::KevyMap;

use crate::StoreError;
use crate::value::{BTREE_SLOT_BYTES, SmallBytes};

mod id;
pub use id::{
    StreamId, StreamIdError, XAddIdSpec, parse_explicit_id, parse_range_end, parse_range_start,
    parse_xadd_id,
};

// ───────────── StreamData ─────────────

/// One stream's storage: every entry in `entries` plus the per-stream
/// scalar state Redis exposes via `XINFO STREAM`, plus the consumer
/// groups map (sprint B). An empty `groups` map costs ~8 bytes and
/// makes the no-group fast path (sprint A XADD/XREAD) zero-overhead.
///
/// ```
/// use kevy_store::{MissingStream, Store, StreamId, XAddIdSpec};
/// let mut s = Store::new();
/// let fields = vec![(b"temp".to_vec(), b"21".to_vec())];
/// s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(5, 0)), fields, MissingStream::Create, 0)?;
/// let stream = s.stream_view(b"s")?.unwrap();
/// assert_eq!((stream.length(), stream.last_id()), (1, StreamId::new(5, 0)));
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Default, Clone)]
pub struct StreamData {
    /// Sorted entries; the `BTreeMap` enforces strict-increasing IDs.
    pub(super) entries: BTreeMap<StreamId, Vec<(SmallBytes, SmallBytes)>>,
    /// Largest ID **ever** seen on this stream, even after the entry
    /// has been deleted (XDEL doesn't roll the clock back).
    pub(super) last_id: StreamId,
    /// Largest ID that has been deleted (`max_deleted_entry_id` in
    /// Redis XINFO). Used to detect "deletion-only" gaps for clients.
    pub(super) max_deleted_id: StreamId,
    /// Cumulative number of entries ever added — never decreases. Used
    /// by XINFO STREAM's `entries-added`.
    pub(super) entries_added: u64,
    /// Consumer groups keyed by name (sprint B). Boxed so the
    /// `StreamData` struct stays compact when no groups are attached.
    pub(super) groups: KevyMap<SmallBytes, Box<group::ConsumerGroup>>,
    /// Where the entries would sit in a Redis server's nodes, for the
    /// approximate trims.
    pub(super) nodes: nodes::Nodes,
}

impl StreamData {
    /// Current entry count (never larger than `entries_added`).
    pub fn length(&self) -> u64 {
        self.entries.len() as u64
    }

    /// Last ID ever assigned. Resets to `MIN` only when the whole key
    /// is deleted (we never down-rev a stream).
    pub fn last_id(&self) -> StreamId {
        self.last_id
    }

    /// XINFO STREAM helpers.
    pub fn entries_added(&self) -> u64 {
        self.entries_added
    }

    /// The highest id XDEL has removed. XINFO reports it, and a consumer
    /// group uses it to tell "never existed" from "existed and was
    /// deleted" when a pending entry cannot be found.
    pub fn max_deleted_id(&self) -> StreamId {
        self.max_deleted_id
    }

    /// Iterate every entry in ID-ascending order. Snapshot serializers
    /// walk this to dump the stream.
    pub fn entries(&self) -> impl Iterator<Item = (StreamId, &[(SmallBytes, SmallBytes)])> {
        self.entries.iter().map(|(id, fv)| (*id, fv.as_slice()))
    }

    /// First (smallest-ID) entry — `None` if empty.
    pub fn first_entry(&self) -> Option<(StreamId, &[(SmallBytes, SmallBytes)])> {
        self.entries.iter().next().map(|(id, fv)| (*id, fv.as_slice()))
    }

    /// Last (largest-ID) entry — `None` if empty.
    pub fn last_entry(&self) -> Option<(StreamId, &[(SmallBytes, SmallBytes)])> {
        self.entries.iter().next_back().map(|(id, fv)| (*id, fv.as_slice()))
    }

    /// Iterate `(group_name, group)` pairs — used by `XINFO GROUPS`.
    pub fn groups(&self) -> impl Iterator<Item = (&[u8], &group::ConsumerGroup)> {
        self.groups.iter().map(|(k, v)| (k.as_slice(), v.as_ref()))
    }

    /// Lookup one group by name (for `XINFO CONSUMERS`).
    pub fn group(&self, name: &[u8]) -> Option<&group::ConsumerGroup> {
        self.groups.get(name).map(core::convert::AsRef::as_ref)
    }

    /// Group count — `XINFO STREAM`'s `groups` field.
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Snapshot-loader entry-point: insert a pre-existing entry without
    /// touching scalar state. Used by `Store::load_stream`; the loader
    /// pumps every entry then calls [`Self::set_loaded_state`] once.
    pub fn load_entry(&mut self, id: StreamId, fields: Vec<(SmallBytes, SmallBytes)>) {
        self.nodes.append(id, &fields);
        self.entries.insert(id, fields);
    }

    /// Snapshot-loader: restore the per-stream scalars after every
    /// entry has been pushed via [`Self::load_entry`].
    pub fn set_loaded_state(
        &mut self,
        last_id: StreamId,
        max_deleted_id: StreamId,
        entries_added: u64,
    ) {
        self.last_id = last_id;
        self.max_deleted_id = max_deleted_id;
        self.entries_added = entries_added;
    }

    /// Insert a pre-resolved entry. Caller is responsible for picking
    /// the ID via [`StreamData::resolve_xadd_id`] so monotonicity holds.
    pub(crate) fn insert(&mut self, id: StreamId, fields: Vec<(SmallBytes, SmallBytes)>) {
        debug_assert!(id > self.last_id || (id == StreamId::MIN && self.last_id == StreamId::MIN));
        self.nodes.append(id, &fields);
        self.entries.insert(id, fields);
        self.last_id = id;
        self.entries_added += 1;
    }

    /// Translate XADD's `XAddIdSpec` into a concrete `StreamId`,
    /// rejecting any spec that would not be strictly greater than
    /// `self.last_id`. `now_ms` is injected so tests can pin wall-clock.
    pub fn resolve_xadd_id(&self, spec: XAddIdSpec, now_ms: u64) -> Result<StreamId, StoreError> {
        let last = self.last_id;
        if last == StreamId::MAX {
            return Err(StoreError::StreamExhausted);
        }
        let candidate = match spec {
            XAddIdSpec::AutoAll if now_ms > last.ms => StreamId::new(now_ms, 0),
            XAddIdSpec::AutoAll => last.next(),
            XAddIdSpec::AutoSeq(ms) if ms > last.ms => StreamId::new(ms, 0),
            XAddIdSpec::AutoSeq(ms) if ms < last.ms || last.seq == u64::MAX => {
                return Err(StoreError::OutOfRange);
            }
            XAddIdSpec::AutoSeq(ms) => StreamId::new(ms, last.seq + 1),
            XAddIdSpec::Explicit(id) if id <= last || id == StreamId::MIN => {
                return Err(StoreError::OutOfRange);
            }
            XAddIdSpec::Explicit(id) => id,
        };
        Ok(candidate)
    }

    /// XRANGE — inclusive `[start, end]`, optionally COUNT-bounded.
    pub fn range(
        &self,
        start: StreamId,
        end: StreamId,
        count: Option<usize>,
    ) -> Vec<(StreamId, &[(SmallBytes, SmallBytes)])> {
        if start > end {
            return Vec::new();
        }
        let iter = self.entries.range(start..=end).map(|(id, fv)| (*id, fv.as_slice()));
        match count {
            Some(n) => iter.take(n).collect(),
            None => iter.collect(),
        }
    }

    /// XREVRANGE — same `[start, end]` interval, descending order.
    pub fn revrange(
        &self,
        start: StreamId,
        end: StreamId,
        count: Option<usize>,
    ) -> Vec<(StreamId, &[(SmallBytes, SmallBytes)])> {
        if start > end {
            return Vec::new();
        }
        let iter = self.entries.range(start..=end).rev().map(|(id, fv)| (*id, fv.as_slice()));
        match count {
            Some(n) => iter.take(n).collect(),
            None => iter.collect(),
        }
    }

    /// XREAD — entries strictly after `last_seen`, optionally COUNT-bounded.
    pub fn read_after(
        &self,
        last_seen: StreamId,
        count: Option<usize>,
    ) -> Vec<(StreamId, &[(SmallBytes, SmallBytes)])> {
        if last_seen == StreamId::MAX {
            return Vec::new();
        }
        self.range(last_seen.next(), StreamId::MAX, count)
    }

    /// XDEL — remove `ids`. Returns the count actually removed (missing
    /// IDs silently skipped). Updates `max_deleted_id` so XINFO can
    /// report it.
    pub(crate) fn del_ids(&mut self, ids: &[StreamId]) -> usize {
        let mut removed = 0usize;
        for id in ids {
            if self.entries.remove(id).is_some() {
                self.nodes.delete(*id);
                removed += 1;
                if *id > self.max_deleted_id {
                    self.max_deleted_id = *id;
                }
            }
        }
        removed
    }

    /// Approximate heap footprint for `Value::weight`. Walks the entry
    /// list once; cheap relative to the size of the stream itself.
    pub fn weight(&self) -> u64 {
        let entry_sum: u64 = self
            .entries
            .values()
            .map(|fv| {
                24 + fv
                    .iter()
                    .map(|(f, v)| 48 + f.heap_bytes() as u64 + v.heap_bytes() as u64)
                    .sum::<u64>()
            })
            .sum();
        (self.entries.len() as u64).saturating_mul(BTREE_SLOT_BYTES) + entry_sum
    }
}

mod claim;
mod group;
mod lag;
mod load;
mod modes;
mod nodes;
mod pending;
mod restore;
mod store;
pub use claim::{AutoclaimResult, XClaimOpts};
pub use group::{ConsumerGroup, ConsumerState, GroupCreateMode, PelEntry, ReadGroupId};
pub use load::{LoadedGroup, LoadedPelEntry};
pub use modes::{AckMode, ClaimMode, MissingStream};
pub use nodes::{APPROX_TRIM_LIMIT, TrimMode, TrimTo};
pub use pending::{PendingExtended, PendingExtendedRow, PendingSummary};
pub use store::{EntryBatch, GroupBatch};

/// Snapshot-loader payload: one stream entry decoded into primitive
/// tuples `(ms, seq, [(field, value), ...])`. The persist crate emits
/// these and `Store::load_stream` consumes them.
///
/// ```
/// use kevy_store::{LoadedStreamEntry, Store, StreamId};
/// let mut s = Store::new();
/// let entries: Vec<LoadedStreamEntry> = vec![(5, 0, vec![(b"f".to_vec(), b"v".to_vec())])];
/// s.load_stream(b"s".to_vec(), entries, (5, 0), (0, 0), 1, Vec::new(), None);
/// assert_eq!(s.xread_dollar_last_id(b"s")?, StreamId::new(5, 0));
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
pub type LoadedStreamEntry = (u64, u64, Vec<(Vec<u8>, Vec<u8>)>);

// ───────────── small helpers (shared with `store.rs`) ─────────────

/// Wall-clock millis. Shared with dispatchers so every XADD on a shard uses
/// the same clock source. On native targets reads `SystemTime::now()` (falls
/// back to 0 on a pre-UNIX-EPOCH clock — impossible on supported platforms);
/// on `wasm32-unknown-unknown`, where `SystemTime::now()` traps, reads the
/// host-fed wall clock (see `crate::set_wall_clock_ms`, wasm-only).
///
/// ```
/// let now = kevy_store::now_unix_ms();
/// assert!(now > 1_600_000_000_000); // after September 2020, in milliseconds
/// ```
#[cfg(not(any(feature = "external-clock", all(target_arch = "wasm32", target_os = "unknown"))))]
pub fn now_unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Wall-clock milliseconds since the epoch, for stream ids.
///
/// The twin of the `SystemTime` version above, for builds that have no
/// `SystemTime`: an external-clock build or wasm. Both must agree on the
/// unit — a stream id is a millisecond and nothing downstream re-scales.
#[cfg(any(feature = "external-clock", all(target_arch = "wasm32", target_os = "unknown")))]
pub fn now_unix_ms() -> u64 {
    crate::clock::wall_now_unix_ms()
}

pub(super) fn stream_entry_weight(fields: &[(SmallBytes, SmallBytes)]) -> u64 {
    // BTreeMap slot + Vec header + each (field, value) cell + their heap.
    BTREE_SLOT_BYTES
        + 24
        + fields
            .iter()
            .map(|(f, v)| 48 + f.heap_bytes() as u64 + v.heap_bytes() as u64)
            .sum::<u64>()
}

pub(super) fn clone_entries(src: Vec<(StreamId, &[(SmallBytes, SmallBytes)])>) -> EntryBatch {
    src.into_iter()
        .map(|(id, fv)| (id, fv.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect()))
        .collect()
}

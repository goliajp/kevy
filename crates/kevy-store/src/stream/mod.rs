//! Redis-compatible Streams storage. Each stream is an append-only log
//! of (ID, field-value-list) entries keyed by a monotonically increasing
//! `<ms>-<seq>` ID. The entries live in a `BTreeMap<StreamId, _>` so
//! range queries are O(log n + k) and the iterator natural order is the
//! ID order (ascending).
//!
//! Sprint A scope: bare stream (no consumer groups). The `StreamData`
//! type carries a `groups` slot reserved for sprint B; this file only
//! implements the entry-side ops.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use kevy_map::KevyMap;

use crate::value::*;
use crate::StoreError;

// ───────────── StreamId ─────────────

/// A stream entry's `<ms>-<seq>` identifier. The `Ord` derivation compares
/// `ms` first then `seq`, which is exactly the monotonic order the protocol
/// requires; same derivation gives `Eq`, `Hash`, and the `BTreeMap` key bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub struct StreamId {
    /// Unix milliseconds timestamp component.
    pub ms: u64,
    /// Per-ms sequence number, 0-based.
    pub seq: u64,
}

impl StreamId {
    /// The numerically smallest ID; XRANGE `-` start.
    pub const MIN: StreamId = StreamId { ms: 0, seq: 0 };
    /// The numerically largest representable ID; XRANGE `+` end.
    pub const MAX: StreamId = StreamId { ms: u64::MAX, seq: u64::MAX };

    /// Render as the canonical `<ms>-<seq>` wire form.
    pub fn encode(self) -> Vec<u8> {
        format!("{}-{}", self.ms, self.seq).into_bytes()
    }

    /// Step one ID past `self`. Saturates at [`Self::MAX`].
    pub fn next(self) -> Self {
        if self.seq < u64::MAX {
            StreamId { ms: self.ms, seq: self.seq + 1 }
        } else if self.ms < u64::MAX {
            StreamId { ms: self.ms + 1, seq: 0 }
        } else {
            StreamId::MAX
        }
    }
}

/// XADD's ID argument: either an explicit `<ms>-<seq>` (both parts may
/// be `*` to auto-fill `seq` only) or fully auto-generate via `*`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XAddIdSpec {
    /// `*` — generate both `ms` (= current wall-clock) and `seq`.
    AutoAll,
    /// `<ms>-*` — caller fixes `ms`, server picks the next free `seq`.
    AutoSeq(u64),
    /// `<ms>-<seq>` — caller fully specifies the ID.
    Explicit(StreamId),
}

/// Parse an XADD ID argument (`*`, `ms`, `ms-*`, `ms-seq`).
pub fn parse_xadd_id(s: &[u8]) -> Result<XAddIdSpec, StreamIdError> {
    if s == b"*" {
        return Ok(XAddIdSpec::AutoAll);
    }
    let txt = std::str::from_utf8(s).map_err(|_| StreamIdError::Invalid)?;
    match txt.split_once('-') {
        None => {
            let ms = txt.parse::<u64>().map_err(|_| StreamIdError::Invalid)?;
            Ok(XAddIdSpec::Explicit(StreamId { ms, seq: 0 }))
        }
        Some((ms_s, seq_s)) => {
            let ms = ms_s.parse::<u64>().map_err(|_| StreamIdError::Invalid)?;
            if seq_s == "*" {
                Ok(XAddIdSpec::AutoSeq(ms))
            } else {
                let seq = seq_s.parse::<u64>().map_err(|_| StreamIdError::Invalid)?;
                Ok(XAddIdSpec::Explicit(StreamId { ms, seq }))
            }
        }
    }
}

/// Parse an XRANGE `start` ID. Accepts `-` (= [`StreamId::MIN`]), bare
/// `ms` (seq=0), and full `ms-seq`.
pub fn parse_range_start(s: &[u8]) -> Result<StreamId, StreamIdError> {
    if s == b"-" {
        return Ok(StreamId::MIN);
    }
    parse_explicit_id(s, /*end=*/ false)
}

/// Parse an XRANGE `end` ID. Accepts `+` (= [`StreamId::MAX`]), bare `ms`
/// (seq=u64::MAX so the entire ms is included), and full `ms-seq`.
pub fn parse_range_end(s: &[u8]) -> Result<StreamId, StreamIdError> {
    if s == b"+" {
        return Ok(StreamId::MAX);
    }
    parse_explicit_id(s, /*end=*/ true)
}

/// Parse a fully-explicit ID for XREAD's per-stream "last-seen" arg
/// (`0`, `0-0`, `5-2`). `$` is handled by the caller (it means "the
/// stream's current `last_id`", which only Store can resolve).
pub fn parse_explicit_id(s: &[u8], end: bool) -> Result<StreamId, StreamIdError> {
    let txt = std::str::from_utf8(s).map_err(|_| StreamIdError::Invalid)?;
    let (ms_s, seq_s) = match txt.split_once('-') {
        Some(p) => p,
        None => (txt, if end { "" } else { "0" }),
    };
    let ms = ms_s.parse::<u64>().map_err(|_| StreamIdError::Invalid)?;
    let seq = if seq_s.is_empty() {
        u64::MAX
    } else {
        seq_s.parse::<u64>().map_err(|_| StreamIdError::Invalid)?
    };
    Ok(StreamId { ms, seq })
}

/// Errors `parse_*_id` may emit. Distinct from `StoreError::NotInteger`
/// so callers can map to the more specific Redis wire shape (`ERR
/// Invalid stream ID specified as stream command argument`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamIdError {
    /// Couldn't parse the bytes as `<ms>[-<seq>]` / `*` / `-` / `+`.
    Invalid,
}

// ───────────── StreamData ─────────────

/// One stream's storage: every entry in `entries` plus the per-stream
/// scalar state Redis exposes via `XINFO STREAM`, plus the consumer
/// groups map (sprint B). An empty `groups` map costs ~8 bytes and
/// makes the no-group fast path (sprint A XADD/XREAD) zero-overhead.
#[derive(Default)]
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

    pub fn max_deleted_id(&self) -> StreamId {
        self.max_deleted_id
    }

    /// Iterate every entry in ID-ascending order. Snapshot serializers
    /// walk this to dump the stream.
    pub fn iter_entries(
        &self,
    ) -> impl Iterator<Item = (StreamId, &[(SmallBytes, SmallBytes)])> {
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
    pub fn groups_iter(&self) -> impl Iterator<Item = (&[u8], &group::ConsumerGroup)> {
        self.groups.iter().map(|(k, v)| (k.as_slice(), v.as_ref()))
    }

    /// Lookup one group by name (for `XINFO CONSUMERS`).
    pub fn group(&self, name: &[u8]) -> Option<&group::ConsumerGroup> {
        self.groups.get(name).map(|b| b.as_ref())
    }

    /// Group count — `XINFO STREAM`'s `groups` field.
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Snapshot-loader entry-point: insert a pre-existing entry without
    /// touching scalar state. Used by `Store::load_stream`; the loader
    /// pumps every entry then calls [`Self::set_loaded_state`] once.
    pub fn load_entry(&mut self, id: StreamId, fields: Vec<(SmallBytes, SmallBytes)>) {
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
        self.entries.insert(id, fields);
        self.last_id = id;
        self.entries_added += 1;
    }

    /// Translate XADD's `XAddIdSpec` into a concrete `StreamId`,
    /// rejecting any spec that would not be strictly greater than
    /// `self.last_id`. `now_ms` is injected so tests can pin wall-clock.
    pub fn resolve_xadd_id(
        &self,
        spec: XAddIdSpec,
        now_ms: u64,
    ) -> Result<StreamId, StoreError> {
        let candidate = match spec {
            XAddIdSpec::AutoAll => {
                let ms = now_ms.max(self.last_id.ms);
                if ms == self.last_id.ms {
                    StreamId { ms, seq: self.last_id.seq + 1 }
                } else {
                    StreamId { ms, seq: 0 }
                }
            }
            XAddIdSpec::AutoSeq(ms) => {
                if ms < self.last_id.ms {
                    return Err(StoreError::OutOfRange);
                }
                if ms == self.last_id.ms {
                    StreamId { ms, seq: self.last_id.seq + 1 }
                } else {
                    StreamId { ms, seq: 0 }
                }
            }
            XAddIdSpec::Explicit(id) => {
                if id <= self.last_id {
                    return Err(StoreError::OutOfRange);
                }
                if id == StreamId::MIN {
                    return Err(StoreError::OutOfRange);
                }
                id
            }
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
                removed += 1;
                if *id > self.max_deleted_id {
                    self.max_deleted_id = *id;
                }
            }
        }
        removed
    }

    /// XTRIM MAXLEN — keep the most recent `n` entries.
    pub(crate) fn trim_maxlen(&mut self, n: usize) -> usize {
        let len = self.entries.len();
        if len <= n {
            return 0;
        }
        let drop = len - n;
        let mut removed = 0;
        let drop_ids: Vec<StreamId> = self.entries.keys().copied().take(drop).collect();
        for id in drop_ids {
            self.entries.remove(&id);
            if id > self.max_deleted_id {
                self.max_deleted_id = id;
            }
            removed += 1;
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

    /// XTRIM MINID — drop every entry with ID < `floor`.
    pub(crate) fn trim_minid(&mut self, floor: StreamId) -> usize {
        let drop_ids: Vec<StreamId> = self
            .entries
            .range(..floor)
            .map(|(id, _)| *id)
            .collect();
        let removed = drop_ids.len();
        for id in drop_ids {
            self.entries.remove(&id);
            if id > self.max_deleted_id {
                self.max_deleted_id = id;
            }
        }
        removed
    }
}

mod claim;
mod group;
mod load;
mod store;
#[allow(unused_imports)]
pub use claim::AutoclaimResult;
pub use load::{LoadedGroup, LoadedPelEntry};
#[allow(unused_imports)]
pub use group::{
    ConsumerGroup, ConsumerState, GroupCreateMode, PelEntry, PendingExtended,
    PendingExtendedRow, PendingSummary, ReadGroupId, XClaimOpts,
};
pub use store::EntryBatch;

/// Snapshot-loader payload: one stream entry decoded into primitive
/// tuples `(ms, seq, [(field, value), ...])`. The persist crate emits
/// these and `Store::load_stream` consumes them.
pub type LoadedStreamEntry = (u64, u64, Vec<(Vec<u8>, Vec<u8>)>);

// ───────────── small helpers (shared with `store.rs`) ─────────────

/// Wall-clock millis (`SystemTime::now`). Shared with dispatchers so
/// every XADD on a shard uses the same clock source. Falls back to 0
/// on a pre-UNIX-EPOCH clock — impossible on supported platforms.
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

pub(super) fn clone_entries(
    src: Vec<(StreamId, &[(SmallBytes, SmallBytes)])>,
) -> EntryBatch {
    src.into_iter()
        .map(|(id, fv)| (id, fv.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect()))
        .collect()
}

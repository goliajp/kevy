//! `XPENDING`: the pending-list summary and its per-entry form. Split
//! from `group.rs` to keep it under the 500-LOC cap.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;

use super::{StreamData, StreamId};

/// Summary form of `XPENDING key group` (only 3 args): total pending,
/// min/max IDs across the PEL, and per-consumer aggregate counts.
///
/// ```
/// # use kevy_store::*;
/// # let mut s = Store::new();
/// # for t in [1, 2] {
/// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
/// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
/// # }
/// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
/// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
/// let sum = s.xpending_summary(b"s", b"g")?.expect("group exists");
/// assert_eq!(sum.total, 2);
/// assert_eq!(sum.by_consumer, [(b"alice".to_vec(), 2)]);
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PendingSummary {
    /// Total pending entries across all consumers.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// s.xack(b"s", b"g", &[StreamId::new(1, 0)])?;
    /// assert_eq!(s.xpending_summary(b"s", b"g")?.unwrap().total, 1);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub total: u64,
    /// Smallest and largest pending IDs, or `None` if the PEL is empty.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// let sum = s.xpending_summary(b"s", b"g")?.unwrap();
    /// assert_eq!(sum.id_range, Some((StreamId::new(1, 0), StreamId::new(2, 0))));
    /// s.xack(b"s", b"g", &[StreamId::new(1, 0), StreamId::new(2, 0)])?;
    /// assert_eq!(s.xpending_summary(b"s", b"g")?.unwrap().id_range, None);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub id_range: Option<(StreamId, StreamId)>,
    /// `(consumer, count)` pairs in arbitrary order.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// let opts = XClaimOpts::default();
    /// s.xclaim(b"s", b"g", b"bob", &[StreamId::new(2, 0)], &opts, 200)?;
    /// let mut by = s.xpending_summary(b"s", b"g")?.unwrap().by_consumer;
    /// by.sort();
    /// assert_eq!(by, [(b"alice".to_vec(), 1), (b"bob".to_vec(), 1)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub by_consumer: Vec<(Vec<u8>, u64)>,
}

/// Extended form of `XPENDING key group [IDLE ms] start end count
/// [consumer]`: one row per matching PEL entry.
///
/// ```
/// # use kevy_store::*;
/// # let mut s = Store::new();
/// # for t in [1, 2] {
/// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
/// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
/// # }
/// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
/// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
/// let ext = s.xpending_extended(b"s", b"g", None, StreamId::MIN, StreamId::MAX, 10, None, 150)?.unwrap();
/// let ids: Vec<StreamId> = ext.rows.iter().map(|r| r.id).collect();
/// assert_eq!(ids, [StreamId::new(1, 0), StreamId::new(2, 0)]);
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PendingExtended {
    /// Per-entry rows in ID-ascending order.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// // COUNT 1 keeps only the lowest pending id
    /// let ext = s.xpending_extended(b"s", b"g", None, StreamId::MIN, StreamId::MAX, 1, None, 150)?.unwrap();
    /// assert_eq!(ext.rows.len(), 1);
    /// assert_eq!(ext.rows[0].id, StreamId::new(1, 0));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub rows: Vec<PendingExtendedRow>,
}

/// One row of the extended XPENDING reply.
///
/// ```
/// # use kevy_store::*;
/// # let mut s = Store::new();
/// # for t in [1, 2] {
/// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
/// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
/// # }
/// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
/// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
/// let row = &s.xpending_extended(b"s", b"g", None, StreamId::MIN, StreamId::MAX, 10, None, 150)?.unwrap().rows[0];
/// assert_eq!((row.id, row.consumer.as_slice()), (StreamId::new(1, 0), b"alice".as_slice()));
/// assert_eq!((row.idle_ms, row.delivery_count), (50, 1));
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PendingExtendedRow {
    /// Entry ID.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// let rows = s.xpending_extended(b"s", b"g", None, StreamId::MIN, StreamId::MAX, 10, None, 150)?.unwrap().rows;
    /// assert_eq!(rows[1].id, StreamId::new(2, 0));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub id: StreamId,
    /// Owning consumer's name.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// let rows = s.xpending_extended(b"s", b"g", None, StreamId::MIN, StreamId::MAX, 10, None, 150)?.unwrap().rows;
    /// assert!(rows.iter().all(|r| r.consumer == b"alice"));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub consumer: Vec<u8>,
    /// Idle time in milliseconds (now - delivery_time_ms).
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// // delivered at 100, asked at 175
    /// let ext = s.xpending_extended(b"s", b"g", None, StreamId::MIN, StreamId::MAX, 10, None, 175)?.unwrap();
    /// assert_eq!(ext.rows[0].idle_ms, 75);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub idle_ms: u64,
    /// Delivery count.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// let opts = XClaimOpts::default();
    /// s.xclaim(b"s", b"g", b"alice", &[StreamId::new(1, 0)], &opts, 120)?;
    /// let rows = s.xpending_extended(b"s", b"g", None, StreamId::MIN, StreamId::MAX, 10, None, 150)?.unwrap().rows;
    /// assert_eq!((rows[0].delivery_count, rows[1].delivery_count), (2, 1));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub delivery_count: u32,
}

impl StreamData {
    /// `XPENDING key group` — the summary form (4-tuple).
    pub fn pending_summary(&self, group: &[u8]) -> Option<PendingSummary> {
        let g = self.groups.get(group)?;
        let total = g.pel.len() as u64;
        let id_range = match (g.pel.keys().next(), g.pel.keys().next_back()) {
            (Some(lo), Some(hi)) => Some((*lo, *hi)),
            _ => None,
        };
        let mut counts: Vec<(Vec<u8>, u64)> = Vec::new();
        for p in g.pel.values() {
            if let Some((_, n)) = counts.iter_mut().find(|(name, _)| name == p.consumer.as_slice())
            {
                *n += 1;
            } else {
                counts.push((p.consumer.to_vec(), 1));
            }
        }
        Some(PendingSummary { total, id_range, by_consumer: counts })
    }

    /// `XPENDING key group [IDLE ms] start end count [consumer]`.
    #[allow(clippy::too_many_arguments)]
    pub fn pending_extended(
        &self,
        group: &[u8],
        idle_min_ms: Option<u64>,
        start: StreamId,
        end: StreamId,
        count: usize,
        consumer_filter: Option<&[u8]>,
        now_ms: u64,
    ) -> Option<PendingExtended> {
        let g = self.groups.get(group)?;
        let mut rows = Vec::with_capacity(count.min(g.pel.len()));
        for (id, p) in g.pel.range(start..=end) {
            if rows.len() >= count {
                break;
            }
            let idle = now_ms.saturating_sub(p.delivery_time_ms);
            if let Some(min) = idle_min_ms
                && idle < min
            {
                continue;
            }
            if let Some(c) = consumer_filter
                && p.consumer.as_slice() != c
            {
                continue;
            }
            rows.push(PendingExtendedRow {
                id: *id,
                consumer: p.consumer.to_vec(),
                idle_ms: idle,
                delivery_count: p.delivery_count,
            });
        }
        Some(PendingExtended { rows })
    }
}

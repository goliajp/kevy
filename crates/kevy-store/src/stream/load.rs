//! Consumer-group exchange types + the `XSETID` scalar setter — the
//! pieces persistence (snapshot v4, AOF rewrite, reshard's `load_value`
//! redistribution) needs to carry group/PEL state across a dump/load
//! boundary. Split from `stream/mod.rs` to stay under the 500-LOC cap.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use alloc::collections::BTreeMap;

use kevy_map::KevyMap;

use super::group::{ConsumerGroup, ConsumerState, PelEntry};
use super::{StreamData, StreamId};
use crate::StoreError;
use crate::value::SmallBytes;

/// One PEL row in primitive form: `(ms, seq, consumer, delivery_time_ms,
/// delivery_count)`. The persist crate serializes these verbatim.
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
/// let lg = &s.stream_view(b"s")?.unwrap().export_groups()[0];
/// let (ms, seq, consumer, delivered_at, count) = lg.pel[0].clone();
/// assert_eq!((ms, seq, consumer.as_slice(), delivered_at, count), (1, 0, b"alice".as_slice(), 100, 1));
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
pub type LoadedPelEntry = (u64, u64, Vec<u8>, u64, u64);

/// One consumer group decoded into primitive tuples — the dump/load wire
/// form shared by snapshot v4, AOF-rewrite filtering, and reshard's
/// in-memory redistribution.
///
/// ```
/// use kevy_store::{LoadedGroup, StreamData};
/// let g = LoadedGroup::new(b"g".to_vec(), (1, 0), vec![(b"c".to_vec(), 5)], vec![(1, 0, b"c".to_vec(), 5, 1)]);
/// let mut s = StreamData::default();
/// s.import_groups(vec![g.clone()]);
/// assert_eq!(s.export_groups(), [g]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct LoadedGroup {
    /// Group name.
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
    /// let lg = &s.stream_view(b"s")?.unwrap().export_groups()[0];
    /// assert_eq!(lg.name, b"g");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub name: Vec<u8>,
    /// `last_delivered_id` as `(ms, seq)`.
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
    /// let lg = &s.stream_view(b"s")?.unwrap().export_groups()[0];
    /// assert_eq!(lg.last_delivered, (2, 0));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub last_delivered: (u64, u64),
    /// `(name, last_seen_ms)` per known consumer. `pel_count` is
    /// recomputed from `pel` on import.
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
    /// let lg = &s.stream_view(b"s")?.unwrap().export_groups()[0];
    /// assert_eq!(lg.consumers, [(b"alice".to_vec(), 100)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub consumers: Vec<(Vec<u8>, u64)>,
    /// Every PEL row, including tombstones (entries XDEL'd while
    /// pending) — snapshot keeps those; AOF rewrite filters them.
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
    /// let lg = &s.stream_view(b"s")?.unwrap().export_groups()[0];
    /// let ids: Vec<(u64, u64)> = lg.pel.iter().map(|p| (p.0, p.1)).collect();
    /// assert_eq!(ids, [(1, 0), (2, 0)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub pel: Vec<LoadedPelEntry>,
    /// The group's read counter (`XINFO`'s `entries-read`), `None` when
    /// unknown.
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
    /// let lg = &s.stream_view(b"s")?.unwrap().export_groups()[0];
    /// assert_eq!(lg.entries_read, Some(2));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub entries_read: Option<u64>,
    /// `(name, last_active_ms)` per consumer that has ever been handed an
    /// entry; a consumer not listed never has.
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
    /// s.xgroup_create_consumer(b"s", b"g", b"bob", 150)?;
    /// let lg = &s.stream_view(b"s")?.unwrap().export_groups()[0];
    /// assert_eq!(lg.active, [(b"alice".to_vec(), 100)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub active: Vec<(Vec<u8>, u64)>,
}

impl LoadedGroup {
    /// A group in exchange form, as a loader decodes it, with an unknown
    /// read counter and no consumer ever handed an entry.
    pub fn new(
        name: Vec<u8>,
        last_delivered: (u64, u64),
        consumers: Vec<(Vec<u8>, u64)>,
        pel: Vec<LoadedPelEntry>,
    ) -> Self {
        Self { name, last_delivered, consumers, pel, entries_read: None, active: Vec::new() }
    }

    /// The same group with its read counter and its consumers' last
    /// active times.
    ///
    /// ```
    /// use kevy_store::{LoadedGroup, StreamData};
    /// let g = LoadedGroup::new(b"g".to_vec(), (1, 0), vec![(b"c".to_vec(), 5)], Vec::new())
    ///     .with_reads(Some(1), vec![(b"c".to_vec(), 4)]);
    /// let mut s = StreamData::default();
    /// s.import_groups(vec![g.clone()]);
    /// assert_eq!(s.export_groups(), [g]);
    /// ```
    #[must_use]
    pub fn with_reads(mut self, entries_read: Option<u64>, active: Vec<(Vec<u8>, u64)>) -> Self {
        self.entries_read = entries_read;
        self.active = active;
        self
    }
}

impl StreamData {
    /// Does an entry with `id` currently exist? AOF rewrite uses this
    /// to tell the pending rows XCLAIM can put back from the ones it cannot.
    pub fn contains_entry(&self, id: StreamId) -> bool {
        self.entries.contains_key(&id)
    }

    /// Dump every group into the primitive exchange form.
    pub fn export_groups(&self) -> Vec<LoadedGroup> {
        self.groups
            .iter()
            .map(|(name, g)| LoadedGroup {
                name: name.to_vec(),
                last_delivered: (g.last_delivered_id.ms, g.last_delivered_id.seq),
                consumers: g
                    .consumers
                    .iter()
                    .map(|(c, cs)| (c.to_vec(), cs.last_seen_ms))
                    .collect(),
                pel: g
                    .pel
                    .iter()
                    .map(|(id, p)| {
                        (id.ms, id.seq, p.consumer.to_vec(), p.delivery_time_ms, p.delivery_count)
                    })
                    .collect(),
                entries_read: g.entries_read,
                active: g
                    .consumers
                    .iter()
                    .filter_map(|(c, cs)| cs.last_active_ms.map(|at| (c.to_vec(), at)))
                    .collect(),
            })
            .collect()
    }

    /// Rebuild the group map from the exchange form (loader-side twin of
    /// [`Self::export_groups`]). Per-consumer `pel_count` is recomputed;
    /// a PEL owner missing from the consumer roster (hand-built or
    /// corrupt file) gets a roster slot rather than a panic.
    pub fn import_groups(&mut self, groups: Vec<LoadedGroup>) {
        for lg in groups {
            let mut consumers: KevyMap<SmallBytes, Box<ConsumerState>> = KevyMap::default();
            for (name, last_seen_ms) in lg.consumers {
                let state = ConsumerState::new(&name, last_seen_ms);
                consumers.insert(SmallBytes::from_vec(name), state);
            }
            let mut pel: BTreeMap<StreamId, PelEntry> = BTreeMap::new();
            for (ms, seq, consumer, delivery_time_ms, delivery_count) in lg.pel {
                let consumer = SmallBytes::from_vec(consumer);
                if consumers.get(consumer.as_slice()).is_none() {
                    consumers.insert(consumer.clone(), ConsumerState::new(consumer.as_slice(), 0));
                }
                if let Some(cs) = consumers.get_mut(consumer.as_slice()) {
                    cs.pel_count += 1;
                }
                pel.insert(
                    StreamId::new(ms, seq),
                    PelEntry { consumer, delivery_time_ms, delivery_count },
                );
            }
            for (name, at) in lg.active {
                if let Some(cs) = consumers.get_mut(name.as_slice()) {
                    cs.last_active_ms = Some(at);
                }
            }
            self.groups.insert(
                SmallBytes::from_vec(lg.name),
                Box::new(ConsumerGroup {
                    last_delivered_id: StreamId::new(lg.last_delivered.0, lg.last_delivered.1),
                    pel,
                    consumers,
                    entries_read: lg.entries_read,
                }),
            );
        }
    }

    /// `XSETID key last-id [ENTRIESADDED n] [MAXDELETEDID id]` — overwrite
    /// the stream's scalar state. Rejects a `last_id` below the current
    /// top entry (Redis: "smaller than the target stream top item").
    pub fn xsetid(
        &mut self,
        last_id: StreamId,
        entries_added: Option<u64>,
        max_deleted_id: Option<StreamId>,
    ) -> Result<(), StoreError> {
        if let Some((top, _)) = self.entries.iter().next_back()
            && last_id < *top
        {
            return Err(StoreError::OutOfRange);
        }
        self.last_id = last_id;
        if let Some(n) = entries_added {
            self.entries_added = n;
        }
        if let Some(id) = max_deleted_id {
            self.max_deleted_id = id;
        }
        Ok(())
    }
}

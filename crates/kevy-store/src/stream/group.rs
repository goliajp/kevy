//! Consumer groups for v2-7 streams (sprint B). The group state lives
//! inside its parent [`crate::stream::StreamData`] so XADD / XDEL can
//! see the group map without an extra lookup. This file owns the
//! types + the in-stream operations; the `Store`-side wrappers live in
//! `stream/store.rs` next to the rest of the public API.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use alloc::collections::BTreeMap;

use kevy_map::KevyMap;

use super::{AckMode, EntryBatch, StreamData, StreamId};
use crate::StoreError;
use crate::value::SmallBytes;

#[path = "group_types.rs"]
mod types;
pub use types::{ConsumerState, GroupCreateMode, PelEntry, ReadGroupId};

/// One consumer group's state. Sorted PEL plus a map of known
/// consumers (with cached pel_count for O(1) XINFO answers).
///
/// Read-only outside the store: each consumer's cached pending count must
/// equal its share of the PEL, so every change goes through the stream
/// commands that keep the two together.
///
/// ```
/// use kevy_store::{AckMode, GroupCreateMode, MissingStream, ReadGroupId, StreamId, Store, XAddIdSpec};
/// let mut s = Store::new();
/// s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(1, 1)), vec![(b"f".to_vec(), b"v".to_vec())], MissingStream::Create, 0).unwrap();
/// s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse).unwrap();
/// s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 10).unwrap();
/// let g = s.stream_group_peek(b"s", b"g").unwrap();
/// assert_eq!(g.last_delivered_id(), StreamId::new(1, 1));
/// assert_eq!(g.pending_entry(StreamId::new(1, 1)).unwrap().consumer.as_slice(), b"alice");
/// assert_eq!(g.consumer(b"alice").unwrap().pending_count(), 1);
/// ```
#[derive(Debug, Clone)]
pub struct ConsumerGroup {
    /// Highest ID delivered to any consumer in this group. Bumped by
    /// XREADGROUP with `>`; settable via XGROUP SETID.
    pub(crate) last_delivered_id: StreamId,
    /// Pending-Entries List: every ID delivered but not yet ACKed.
    /// Sorted by ID for `XPENDING start end` range queries.
    pub(crate) pel: BTreeMap<StreamId, PelEntry>,
    /// Consumers known to this group (by name).
    pub(crate) consumers: KevyMap<SmallBytes, Box<ConsumerState>>,
    /// The read counter, `XINFO`'s `entries-read`: how many of the
    /// entries ever added the group has read past. `None` when unknown.
    pub(crate) entries_read: Option<u64>,
}

impl ConsumerGroup {
    /// Highest ID delivered by this group — for `XINFO GROUPS`.
    pub fn last_delivered_id(&self) -> StreamId {
        self.last_delivered_id
    }
    /// `XINFO GROUPS`' `entries-read`: how many of the entries ever added
    /// the group has read past, `None` when a deletion or a jump of its
    /// position (`XGROUP SETID`) made that unknown.
    ///
    /// ```
    /// use kevy_store::{AckMode, GroupCreateMode, MissingStream, ReadGroupId, StreamId, Store, XAddIdSpec};
    /// let mut s = Store::new();
    /// for ms in 1..=3 {
    ///     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    ///     s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(ms, 0)), f, MissingStream::Create, 0)?;
    /// }
    /// s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// assert_eq!(s.stream_group_peek(b"s", b"g").unwrap().entries_read(), None);
    /// s.xreadgroup(b"s", b"g", b"c", ReadGroupId::New, Some(2), AckMode::Pending, 10)?;
    /// assert_eq!(s.stream_group_peek(b"s", b"g").unwrap().entries_read(), Some(2));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn entries_read(&self) -> Option<u64> {
        self.entries_read
    }
    /// Total pending entries — `XINFO GROUPS`'s `pending`.
    pub fn pending_count(&self) -> usize {
        self.pel.len()
    }
    /// The pending entry for `id`, if it is pending.
    pub fn pending_entry(&self, id: StreamId) -> Option<&PelEntry> {
        self.pel.get(&id)
    }
    /// The pending entries with IDs in `range`, in ID order.
    ///
    /// ```
    /// let g = kevy_store::ConsumerGroup::default();
    /// assert_eq!(g.pending_range(..).count(), 0);
    /// ```
    pub fn pending_range(
        &self,
        range: impl core::ops::RangeBounds<StreamId>,
    ) -> impl Iterator<Item = (StreamId, &PelEntry)> {
        self.pel.range(range).map(|(id, p)| (*id, p))
    }
    /// Known consumer count — `XINFO GROUPS`'s `consumers`.
    pub fn consumer_count(&self) -> usize {
        self.consumers.len()
    }
    /// The consumer named `name`, if the group knows it.
    pub fn consumer(&self, name: &[u8]) -> Option<&ConsumerState> {
        self.consumers.get(name).map(AsRef::as_ref)
    }
    /// Iterate `(consumer_name, consumer)` pairs — `XINFO CONSUMERS`.
    pub fn consumers(&self) -> impl Iterator<Item = (&[u8], &ConsumerState)> {
        self.consumers.iter().map(|(k, v)| (k.as_slice(), v.as_ref()))
    }
}

impl Default for ConsumerGroup {
    fn default() -> Self {
        Self {
            last_delivered_id: StreamId::MIN,
            pel: BTreeMap::new(),
            consumers: KevyMap::default(),
            entries_read: None,
        }
    }
}

impl StreamData {
    /// `XGROUP CREATE key group <id|$> [MKSTREAM]`. Returns `true` if
    /// a new group was created; `false` if the group already existed
    /// (caller should report Redis's `-BUSYGROUP` error in that case).
    pub fn group_create(&mut self, name: &[u8], mode: GroupCreateMode) -> Result<bool, StoreError> {
        if self.groups.contains_key(name) {
            return Ok(false);
        }
        let last_delivered_id = match mode {
            GroupCreateMode::AtId(id) => id,
            GroupCreateMode::AtCurrent => self.last_id,
        };
        self.groups.insert(
            SmallBytes::from_slice(name),
            Box::new(ConsumerGroup {
                last_delivered_id,
                pel: BTreeMap::new(),
                consumers: KevyMap::default(),
                entries_read: None,
            }),
        );
        Ok(true)
    }

    /// `XGROUP DESTROY key group`. Returns `true` if a group was dropped.
    pub fn group_destroy(&mut self, name: &[u8]) -> bool {
        self.groups.remove(name).is_some()
    }

    /// `XGROUP SETID key group <id|$>`, which leaves the read counter
    /// unknown. Returns `false` if the group doesn't exist.
    pub fn group_setid(&mut self, name: &[u8], mode: GroupCreateMode) -> bool {
        let Some(g) = self.groups.get_mut(name) else {
            return false;
        };
        g.last_delivered_id = match mode {
            GroupCreateMode::AtId(id) => id,
            GroupCreateMode::AtCurrent => self.last_id,
        };
        g.entries_read = None;
        true
    }

    /// `XGROUP CREATECONSUMER key group consumer TIME seen`: the
    /// consumer's last contact with the group set to `seen_ms` (unix ms),
    /// the consumer created with it when missing. Returns `true` if it was
    /// created, `false` if it existed or the group is missing.
    ///
    /// ```
    /// use kevy_store::{GroupCreateMode, StreamId, XAddIdSpec};
    /// let mut s = kevy_store::Store::new();
    /// s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(1, 1)), vec![(b"f".to_vec(), b"v".to_vec())], kevy_store::MissingStream::Create, 0).unwrap();
    /// s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), kevy_store::MissingStream::Refuse).unwrap();
    /// assert!(s.xgroup_consumer_seen(b"s", b"g", b"c", 40).unwrap());
    /// assert!(!s.xgroup_consumer_seen(b"s", b"g", b"c", 90).unwrap());
    /// let g = s.stream_group_peek(b"s", b"g").unwrap();
    /// assert_eq!(g.consumers().next().unwrap().1.last_seen_ms(), 90);
    /// ```
    pub fn group_consumer_seen(&mut self, group: &[u8], consumer: &[u8], seen_ms: u64) -> bool {
        let Some(g) = self.groups.get_mut(group) else {
            return false;
        };
        if let Some(cs) = g.consumers.get_mut(consumer) {
            cs.last_seen_ms = seen_ms;
            return false;
        }
        g.consumers.insert(SmallBytes::from_slice(consumer), ConsumerState::new(consumer, seen_ms));
        true
    }

    /// Set a consumer's last active time (`None` = never), the consumer
    /// made when missing with its last contact at the same time. `false`
    /// if the group is missing.
    pub fn group_consumer_active(
        &mut self,
        group: &[u8],
        consumer: &[u8],
        active_ms: Option<u64>,
    ) -> bool {
        let Some(g) = self.groups.get_mut(group) else {
            return false;
        };
        if g.consumers.get(consumer).is_none() {
            let seen = active_ms.unwrap_or(0);
            g.consumers
                .insert(SmallBytes::from_slice(consumer), ConsumerState::new(consumer, seen));
        }
        if let Some(cs) = g.consumers.get_mut(consumer) {
            cs.last_active_ms = active_ms;
        }
        true
    }

    /// `XGROUP CREATECONSUMER key group consumer`. Returns `true` if a
    /// new consumer was inserted, `false` if it already existed or the
    /// group is missing.
    pub fn group_create_consumer(&mut self, group: &[u8], consumer: &[u8], now_ms: u64) -> bool {
        let Some(g) = self.groups.get_mut(group) else {
            return false;
        };
        if g.consumers.contains_key(consumer) {
            return false;
        }
        g.consumers.insert(SmallBytes::from_slice(consumer), ConsumerState::new(consumer, now_ms));
        true
    }

    /// `XGROUP DELCONSUMER key group consumer`. Returns the number of
    /// PEL entries dropped along with the consumer (matches Redis).
    pub fn group_del_consumer(&mut self, group: &[u8], consumer: &[u8]) -> u64 {
        let Some(g) = self.groups.get_mut(group) else {
            return 0;
        };
        let dropped = g.pel.len();
        g.pel.retain(|_, p| p.consumer.as_slice() != consumer);
        let dropped = dropped - g.pel.len();
        g.consumers.remove(consumer);
        dropped as u64
    }

    /// `XREADGROUP GROUP g c [COUNT n] STREAMS key id`. ID `>` →
    /// "new entries since last_delivered_id" (updates last_delivered);
    /// ID `<x>` → "PEL entries for this consumer with id > x" (does
    /// NOT update last_delivered, used for replay).
    pub fn readgroup(
        &mut self,
        group: &[u8],
        consumer: &[u8],
        last_seen_arg: ReadGroupId,
        count: Option<usize>,
        ack: AckMode,
        now_ms: u64,
    ) -> Result<EntryBatch, StoreError> {
        let Some(g) = self.groups.get_mut(group) else {
            return Err(StoreError::NoSuchKey);
        };
        let consumer_smb = SmallBytes::from_slice(consumer);
        seen_consumer(g, &consumer_smb, now_ms);
        match last_seen_arg {
            ReadGroupId::New => {
                let start = g.last_delivered_id.next();
                let take: Vec<(StreamId, &[(SmallBytes, SmallBytes)])> = self
                    .entries
                    .range(start..=StreamId::MAX)
                    .map(|(id, fv)| (*id, fv.as_slice()))
                    .take(count.unwrap_or(usize::MAX))
                    .collect();
                let Some(&(to, _)) = take.last() else {
                    return Ok(Vec::new());
                };
                if ack == AckMode::Pending {
                    record_deliveries(g, &consumer_smb, &take, now_ms);
                    if let Some(cs) = g.consumers.get_mut(consumer_smb.as_slice()) {
                        cs.last_active_ms = Some(now_ms);
                    }
                }
                let tally = super::lag::Tally::of(
                    &self.entries,
                    self.entries_added,
                    self.max_deleted_id,
                    self.last_id,
                );
                let n = take.len() as u64;
                g.entries_read = tally.after_read(g.last_delivered_id, g.entries_read, n, to);
                g.last_delivered_id = to;
                Ok(super::clone_entries(take))
            }
            ReadGroupId::ReplayAfter(after) => {
                Ok(replay_pel_entries(g, &self.entries, &consumer_smb, after, count))
            }
        }
    }

    /// `XACK key group id [...]`. Returns count of PEL entries removed.
    pub fn ack(&mut self, group: &[u8], ids: &[StreamId]) -> u64 {
        let Some(g) = self.groups.get_mut(group) else {
            return 0;
        };
        let mut n = 0u64;
        for id in ids {
            if let Some(p) = g.pel.remove(id) {
                if let Some(cs) = g.consumers.get_mut(p.consumer.as_slice()) {
                    cs.pel_count = cs.pel_count.saturating_sub(1);
                }
                n += 1;
            }
        }
        n
    }
}

/// Idempotent insert: ensure the named consumer exists in this group's
/// roster so subsequent `pel_count`/`last_seen_ms` updates have a slot.
/// The `XREADGROUP … <id>` replay arm: PEL entries owned by `consumer`
/// with id strictly after `after`, joined against the live entry map
/// (XDEL'd tombstones are skipped), capped at `count`.
fn replay_pel_entries(
    g: &ConsumerGroup,
    entries: &alloc::collections::BTreeMap<StreamId, Vec<(SmallBytes, SmallBytes)>>,
    consumer: &SmallBytes,
    after: StreamId,
    count: Option<usize>,
) -> EntryBatch {
    let mut hit: Vec<(StreamId, Vec<(SmallBytes, SmallBytes)>)> = Vec::new();
    for (id, pel_entry) in g.pel.range(after.next()..=StreamId::MAX) {
        if pel_entry.consumer != *consumer {
            continue;
        }
        if let Some(fv) = entries.get(id) {
            hit.push((*id, fv.clone()));
        }
        if let Some(n) = count
            && hit.len() >= n
        {
            break;
        }
    }
    hit.into_iter()
        .map(|(id, fv)| (id, fv.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect()))
        .collect()
}

/// Make the named consumer if missing, and note `now_ms` as its contact.
fn seen_consumer(g: &mut ConsumerGroup, name: &SmallBytes, now_ms: u64) {
    ensure_consumer(g, name, now_ms);
    if let Some(cs) = g.consumers.get_mut(name.as_slice()) {
        cs.last_seen_ms = now_ms;
    }
}

pub(super) fn ensure_consumer(g: &mut ConsumerGroup, name: &SmallBytes, now_ms: u64) {
    if g.consumers.get(name.as_slice()).is_none() {
        g.consumers.insert(name.clone(), ConsumerState::new(name.as_slice(), now_ms));
    }
}

fn record_deliveries(
    g: &mut ConsumerGroup,
    consumer: &SmallBytes,
    entries: &[(StreamId, &[(SmallBytes, SmallBytes)])],
    now_ms: u64,
) {
    let mut new_for_consumer = 0usize;
    for (id, _) in entries {
        let entry = g.pel.entry(*id).or_insert_with(|| {
            new_for_consumer += 1;
            PelEntry { consumer: consumer.clone(), delivery_time_ms: now_ms, delivery_count: 0 }
        });
        if entry.consumer != *consumer {
            // Ownership transfer via the read path is unusual; Redis
            // does it on `>` reads only when the PEL already had an
            // entry from a previous owner — treat as XCLAIM-style.
            if let Some(prev) = g.consumers.get_mut(entry.consumer.as_slice()) {
                prev.pel_count = prev.pel_count.saturating_sub(1);
            }
            entry.consumer = consumer.clone();
            new_for_consumer += 1;
        }
        entry.delivery_time_ms = now_ms;
        entry.delivery_count = entry.delivery_count.saturating_add(1);
    }
    if let Some(cs) = g.consumers.get_mut(consumer.as_slice()) {
        cs.pel_count = cs.pel_count.saturating_add(new_for_consumer);
    }
}

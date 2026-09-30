//! A consumer group's read counter (`XINFO`'s `entries-read`) and the
//! `lag` it gives: how many of the stream's entries the group has yet to
//! be handed.
//!
//! The counter is the number of entries ever added that the group has
//! read past. The stream keeps no position for an arbitrary ID, so the
//! counter is known exactly only where the stream's own scalars pin it
//! down, and it is dropped (`None`, answered as nil) when a deletion may
//! lie in the span it would have to count across:
//!
//! * at the last ID ever added, it is `entries-added`;
//! * before the first live entry, or on it, it is the entries no longer
//!   in the stream (plus one on it), provided no deletion lies among the
//!   live entries and none lies between the ID and the first live entry
//!   (`0-0`, a group that has read nothing, is exempt from the second);
//! * anywhere else it is carried forward from a known value by counting
//!   the entries a read delivers, while no deletion lies after the
//!   group's position.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use alloc::collections::BTreeMap;

use super::group::ConsumerGroup;
use super::{StreamData, StreamId};
use crate::value::SmallBytes;
use crate::{Store, StoreError};

/// The stream scalars the counter is worked out from, copied out so a
/// group can be borrowed mutably beside them.
#[derive(Clone, Copy, Debug)]
pub(super) struct Tally {
    added: u64,
    length: u64,
    max_deleted: StreamId,
    last: StreamId,
    first: Option<StreamId>,
}

impl Tally {
    pub(super) fn of(
        entries: &BTreeMap<StreamId, Vec<(SmallBytes, SmallBytes)>>,
        added: u64,
        max_deleted: StreamId,
        last: StreamId,
    ) -> Tally {
        let first = entries.first_key_value().map(|(id, _)| *id);
        Tally { added, length: entries.len() as u64, max_deleted, last, first }
    }

    /// Whether an entry deleted from the stream may sit after `id`, so a
    /// counter kept at `id` may count an entry that is gone.
    fn deleted_after(&self, id: StreamId) -> bool {
        self.length > 0 && self.max_deleted != StreamId::MIN && self.max_deleted >= id
    }

    /// The counter at `id` when the stream alone tells it.
    fn counter_at(&self, id: StreamId) -> Option<u64> {
        if id > self.last {
            return None;
        }
        if id == self.last {
            return Some(self.added);
        }
        let gone = self.added.saturating_sub(self.length);
        let Some(first) = self.first else {
            return (id == StreamId::MIN || id >= self.max_deleted).then_some(gone);
        };
        if self.max_deleted != StreamId::MIN && self.max_deleted >= first {
            return None;
        }
        if id < first {
            return (id == StreamId::MIN || id >= self.max_deleted).then_some(gone);
        }
        (id == first).then_some(gone + 1)
    }

    /// `lag` for a group at `last_delivered` holding `entries_read`.
    pub(super) fn lag(&self, last_delivered: StreamId, entries_read: Option<u64>) -> Option<i64> {
        if self.added == 0 {
            return Some(0);
        }
        let read = match entries_read {
            Some(n) if !self.deleted_after(last_delivered) => n,
            // an empty stream leaves nothing to hand to a group at or
            // before its last ID, wherever the deletions fell
            None if self.length == 0 && last_delivered <= self.last => self.added,
            _ => self.counter_at(last_delivered)?,
        };
        Some(difference(self.added, read))
    }

    /// The counter after a read moved a group from `from` to `to`,
    /// delivering `n` entries, the counter having been `entries_read`.
    pub(super) fn after_read(
        &self,
        from: StreamId,
        entries_read: Option<u64>,
        n: u64,
        to: StreamId,
    ) -> Option<u64> {
        // from before the first live entry, the read took the first `n`,
        // and with no deletion among the live entries each one counts
        if let Some(first) = self.first
            && from < first
            && (self.max_deleted == StreamId::MIN || self.max_deleted < first)
        {
            return Some(self.added.saturating_sub(self.length).saturating_add(n));
        }
        match entries_read {
            Some(read) if !self.deleted_after(from) => Some(read.saturating_add(n)),
            _ => self.counter_at(to),
        }
    }
}

/// `a - b` as a reply integer. A counter set past what the stream holds
/// (`ENTRIESREAD` accepts any value) gives a negative lag.
fn difference(a: u64, b: u64) -> i64 {
    let d = i128::from(a) - i128::from(b);
    d.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

impl StreamData {
    pub(super) fn tally(&self) -> Tally {
        Tally::of(&self.entries, self.entries_added, self.max_deleted_id, self.last_id)
    }

    /// `XINFO`'s `lag` for `group`: the entries it has yet to be handed,
    /// `None` when a deletion makes that unknowable (answered as nil).
    ///
    /// ```
    /// use kevy_store::{GroupCreateMode, MissingStream, Store, StreamId, XAddIdSpec};
    /// let mut s = Store::new();
    /// for ms in 1..=3 {
    ///     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    ///     s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(ms, 0)), f, MissingStream::Create, 0)?;
    /// }
    /// s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// let stream = s.stream_view(b"s")?.unwrap();
    /// assert_eq!(stream.group_lag(stream.group(b"g").unwrap()), Some(3));
    /// s.xdel(b"s", &[StreamId::new(2, 0)])?;
    /// let stream = s.stream_view(b"s")?.unwrap();
    /// assert_eq!(stream.group_lag(stream.group(b"g").unwrap()), None, "a deleted entry ahead");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn group_lag(&self, group: &ConsumerGroup) -> Option<i64> {
        self.tally().lag(group.last_delivered_id, group.entries_read)
    }

    /// Set a group's read counter, as `XGROUP CREATE | SETID …
    /// ENTRIESREAD n` does (`None` = unknown). `false` if the group is
    /// missing.
    pub fn group_set_entries_read(&mut self, group: &[u8], entries_read: Option<u64>) -> bool {
        let Some(g) = self.groups.get_mut(group) else {
            return false;
        };
        g.entries_read = entries_read;
        true
    }
}

impl Store {
    /// `ENTRIESREAD` of `XGROUP CREATE | SETID`: see
    /// [`StreamData::group_set_entries_read`]. `false` on a missing key
    /// or group.
    ///
    /// ```
    /// use kevy_store::{GroupCreateMode, MissingStream, Store};
    /// let mut s = Store::new();
    /// s.xgroup_create(b"s", b"g", GroupCreateMode::AtCurrent, MissingStream::Create)?;
    /// assert!(s.xgroup_set_entries_read(b"s", b"g", Some(7))?);
    /// assert_eq!(s.stream_group_peek(b"s", b"g").unwrap().entries_read(), Some(7));
    /// assert!(!s.xgroup_set_entries_read(b"s", b"missing", None)?);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn xgroup_set_entries_read(
        &mut self,
        key: &[u8],
        group: &[u8],
        entries_read: Option<u64>,
    ) -> Result<bool, StoreError> {
        let set = match self.stream_mut(key, false)? {
            Some(s) => s.group_set_entries_read(group, entries_read),
            None => false,
        };
        if set {
            self.bump_if_watched(key);
        }
        Ok(set)
    }

    /// Set a pending entry's delivery count, as a snapshot loader restores
    /// one. `false` if the key, the group or the pending entry is missing.
    ///
    /// ```
    /// use kevy_store::*;
    /// let mut s = Store::new();
    /// let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(1, 0)), f, MissingStream::Create, 0)?;
    /// s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// s.xreadgroup(b"s", b"g", b"c", ReadGroupId::New, None, AckMode::Pending, 5)?;
    /// assert!(s.xgroup_set_delivery_count(b"s", b"g", StreamId::new(1, 0), 1 << 40)?);
    /// let g = s.stream_group_peek(b"s", b"g").unwrap();
    /// assert_eq!(g.pending_entry(StreamId::new(1, 0)).unwrap().delivery_count, 1 << 40);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn xgroup_set_delivery_count(
        &mut self,
        key: &[u8],
        group: &[u8],
        id: StreamId,
        count: u64,
    ) -> Result<bool, StoreError> {
        let Some(s) = self.stream_mut(key, false)? else {
            return Ok(false);
        };
        let Some(p) = s.groups.get_mut(group).and_then(|g| g.pel.get_mut(&id)) else {
            return Ok(false);
        };
        p.delivery_count = count;
        Ok(true)
    }

    /// Set a consumer's last active time (`None` = never), making the
    /// consumer when missing. A replay applies this from a record, so the
    /// time a consumer was last handed an entry survives a restart.
    /// `false` if the key or group is missing.
    ///
    /// ```
    /// use kevy_store::{GroupCreateMode, MissingStream, Store};
    /// let mut s = Store::new();
    /// s.xgroup_create(b"s", b"g", GroupCreateMode::AtCurrent, MissingStream::Create)?;
    /// assert!(s.xgroup_consumer_active(b"s", b"g", b"c", Some(40))?);
    /// let g = s.stream_group_peek(b"s", b"g").unwrap();
    /// assert_eq!(g.consumer(b"c").unwrap().last_active_ms(), Some(40));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn xgroup_consumer_active(
        &mut self,
        key: &[u8],
        group: &[u8],
        consumer: &[u8],
        active_ms: Option<u64>,
    ) -> Result<bool, StoreError> {
        let Some(s) = self.stream_mut(key, false)? else {
            return Ok(false);
        };
        Ok(s.group_consumer_active(group, consumer, active_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(ms: u64) -> StreamId {
        StreamId::new(ms, 0)
    }

    /// Live entries `live`, `added` ever added, the highest deletion at
    /// `deleted` (0 = none), the last ID ever given `last`.
    fn tally(live: &[u64], added: u64, deleted: u64, last: u64) -> Tally {
        let entries: BTreeMap<StreamId, Vec<(SmallBytes, SmallBytes)>> =
            live.iter().map(|ms| (id(*ms), Vec::new())).collect();
        Tally::of(&entries, added, id(deleted), id(last))
    }

    #[test]
    fn without_deletions_the_counter_is_exact_everywhere_it_is_known() {
        let t = tally(&[1, 2, 3, 4, 5], 5, 0, 5);
        assert_eq!(t.lag(id(2), Some(2)), Some(3));
        assert_eq!(t.lag(StreamId::MIN, None), Some(5), "nothing read yet");
        assert_eq!(t.lag(id(1), None), Some(4), "on the first entry");
        assert_eq!(t.lag(id(2), None), None, "mid-stream with no counter");
        assert_eq!(t.lag(id(5), None), Some(0), "at the last entry");
        assert_eq!(t.lag(id(9), None), None, "past the last entry");
        assert_eq!(t.lag(id(2), Some(10)), Some(-5), "a counter set too high");
    }

    #[test]
    fn a_deletion_after_the_position_drops_the_counter() {
        // 1..=5 added, 4 deleted
        let t = tally(&[1, 2, 3, 5], 5, 4, 5);
        assert_eq!(t.lag(id(2), Some(2)), None);
        assert_eq!(t.lag(id(4), Some(4)), None, "a deletion at the position counts");
        assert_eq!(t.lag(StreamId::MIN, None), None, "a deletion among the live entries");
        assert_eq!(t.lag(id(5), None), Some(0));
        assert_eq!(t.after_read(id(2), Some(2), 1, id(3)), None);
        assert_eq!(t.after_read(StreamId::MIN, None, 4, id(5)), Some(5), "read to the end");
    }

    #[test]
    fn deletions_before_the_first_live_entry_leave_the_head_known() {
        // 1..=5 added, 1..=3 deleted
        let t = tally(&[4, 5], 5, 3, 5);
        assert_eq!(t.lag(StreamId::MIN, None), Some(2));
        assert_eq!(t.lag(id(1), None), None, "a deletion between it and the head");
        assert_eq!(t.lag(id(3), None), Some(2));
        assert_eq!(t.lag(id(4), None), Some(1));
        assert_eq!(t.lag(StreamId::MIN, Some(0)), Some(2), "the counter is not trusted");
        assert_eq!(t.after_read(StreamId::MIN, Some(0), 1, id(4)), Some(4));
        assert_eq!(t.after_read(id(1), Some(1), 1, id(4)), Some(4), "landed on the head");
    }

    #[test]
    fn a_read_from_before_the_head_counts_from_the_head() {
        let t = tally(&[1, 2, 3], 3, 0, 3);
        assert_eq!(t.after_read(StreamId::MIN, Some(3), 1, id(1)), Some(1));
        assert_eq!(t.after_read(id(1), Some(7), 1, id(2)), Some(8));
        assert_eq!(t.after_read(id(1), None, 1, id(2)), None);
    }

    #[test]
    fn an_empty_stream() {
        assert_eq!(tally(&[], 0, 0, 0).lag(StreamId::MIN, Some(3)), Some(0), "never written");
        let t = tally(&[], 2, 2, 2);
        assert_eq!(t.lag(StreamId::MIN, Some(5)), Some(-3));
        assert_eq!(t.lag(StreamId::MIN, None), Some(0));
        assert_eq!(t.lag(id(9), None), None);
    }

    #[test]
    fn an_emptied_stream_knows_the_counter_only_past_its_last_deletion() {
        // three added, all gone, the highest deletion at 2
        let t = tally(&[], 3, 2, 3);
        assert_eq!(t.lag(id(1), None), None, "before the last deletion");
        assert_eq!(t.lag(id(2), None), Some(0), "at it: everything is gone and read");
    }

    /// The store-level setters answer a missing key, a key of another type,
    /// a missing group and a consumer that does not exist yet.
    #[test]
    fn the_read_setters_answer_every_missing_thing() {
        use crate::{GroupCreateMode, MissingStream, SetCondition, Store, XAddIdSpec};
        let mut s = Store::new();
        assert_eq!(s.xgroup_set_entries_read(b"none", b"g", Some(1)), Ok(false));
        assert_eq!(s.xgroup_consumer_active(b"none", b"g", b"c", Some(1)), Ok(false));
        s.set(b"str", b"v".to_vec(), None, SetCondition::Always);
        assert!(s.xgroup_set_entries_read(b"str", b"g", Some(1)).is_err());
        assert!(s.xgroup_consumer_active(b"str", b"g", b"c", Some(1)).is_err());
        let f = vec![(b"f".to_vec(), b"v".to_vec())];
        s.xadd(b"s", XAddIdSpec::Explicit(id(1)), f, MissingStream::Create, 0).unwrap();
        assert_eq!(s.xgroup_set_entries_read(b"s", b"g", Some(1)), Ok(false), "no group");
        assert_eq!(s.xgroup_consumer_active(b"s", b"g", b"c", Some(1)), Ok(false), "no group");
        let g = GroupCreateMode::AtId(StreamId::MIN);
        s.xgroup_create(b"s", b"g", g, MissingStream::Refuse).unwrap();
        assert_eq!(s.xgroup_consumer_active(b"s", b"g", b"new", Some(7)), Ok(true));
        let group = s.stream_group_peek(b"s", b"g").unwrap();
        let (_, made) = group.consumers().find(|(n, _)| *n == b"new").unwrap();
        assert_eq!(made.last_active_ms(), Some(7), "made, with the time it was active");
    }

    /// A group rebuilt from its loaded form keeps its counter and active
    /// times; a PEL entry names a consumer the list left out, which is
    /// made, and an active time for a consumer that is not there is dropped.
    #[test]
    fn a_loaded_group_carries_its_reads() {
        use crate::{LoadedGroup, StreamData};
        let pel = vec![(1, 0, b"only-in-pel".to_vec(), 5, 1)];
        let g = LoadedGroup::new(b"g".to_vec(), (1, 0), vec![(b"c".to_vec(), 5)], pel)
            .with_reads(Some(1), vec![(b"c".to_vec(), 4), (b"gone".to_vec(), 3)]);
        let mut s = StreamData::default();
        s.import_groups(vec![g]);
        let group = s.group(b"g").unwrap();
        assert_eq!(group.entries_read(), Some(1));
        let active: Vec<_> =
            group.consumers().map(|(n, c)| (n.to_vec(), c.last_active_ms())).collect();
        assert!(active.contains(&(b"c".to_vec(), Some(4))), "{active:?}");
        assert!(active.contains(&(b"only-in-pel".to_vec(), None)), "{active:?}");
        assert!(!active.iter().any(|(n, _)| n == b"gone"), "{active:?}");
        let empty = crate::ConsumerGroup::default();
        assert_eq!((empty.entries_read(), empty.consumers().count()), (None, 0));
    }
}

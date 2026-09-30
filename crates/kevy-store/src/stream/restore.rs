//! Putting back a pending entry as a rewritten log holds it, whether or
//! not the stream still holds the entry it names.

use super::StreamId;
use super::group::{PelEntry, ensure_consumer};
use crate::value::SmallBytes;
use crate::{Store, StoreError};

impl Store {
    /// Put a pending entry back: `id` pending for `consumer` since
    /// `delivery_time_ms`, delivered `delivery_count` times, replacing
    /// any row for `id` and making the consumer when missing. The entry
    /// may be gone from the stream: a history read hands it back with no
    /// fields and `XAUTOCLAIM` clears it, as for any deleted entry.
    /// `false` if the key or group is missing.
    ///
    /// ```
    /// use kevy_store::{GroupCreateMode, MissingStream, Store, StreamId};
    /// let mut s = Store::new();
    /// s.xgroup_create(b"s", b"g", GroupCreateMode::AtCurrent, MissingStream::Create)?;
    /// assert!(s.xgroup_restore_pending(b"s", b"g", b"c", StreamId::new(5, 0), 40, 3)?);
    /// let g = s.stream_group_peek(b"s", b"g").unwrap();
    /// let row = g.pending_entry(StreamId::new(5, 0)).unwrap();
    /// assert_eq!((row.consumer.as_slice(), row.delivery_time_ms, row.delivery_count), (&b"c"[..], 40, 3));
    /// assert_eq!(g.consumer(b"c").unwrap().pending_count(), 1);
    /// assert!(!s.xgroup_restore_pending(b"s", b"none", b"c", StreamId::new(5, 0), 40, 3)?);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn xgroup_restore_pending(
        &mut self,
        key: &[u8],
        group: &[u8],
        consumer: &[u8],
        id: StreamId,
        delivery_time_ms: u64,
        delivery_count: u64,
    ) -> Result<bool, StoreError> {
        let Some(s) = self.stream_mut(key, false)? else {
            return Ok(false);
        };
        let Some(g) = s.groups.get_mut(group) else {
            return Ok(false);
        };
        let owner = SmallBytes::from_slice(consumer);
        ensure_consumer(g, &owner, 0);
        let row = PelEntry { consumer: owner.clone(), delivery_time_ms, delivery_count };
        let prev = g.pel.insert(id, row);
        super::claim::transfer_ownership_counts(g, prev.as_ref(), &owner);
        self.bump_if_watched(key);
        Ok(true)
    }
}

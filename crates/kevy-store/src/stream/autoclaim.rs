//! `XAUTOCLAIM`: walk the pending list from a cursor, claiming what has
//! been idle long enough and dropping what the stream no longer holds.

use super::{AutoclaimResult, XClaimOpts, claim_one};
use crate::StoreError;
use crate::stream::{ClaimMode, StreamData, StreamId};
use crate::value::SmallBytes;

impl StreamData {
    /// `XAUTOCLAIM key group consumer min-idle-ms start [COUNT n]
    /// [JUSTID]`. Walks the PEL from `start`, looking at no more than
    /// `count × 10` entries. An entry the stream no longer holds is
    /// dropped from the PEL and reported, however idle it is; one idle for
    /// at least `min_idle_ms` is claimed; both count towards `count`. The
    /// cursor is the next pending entry's id, or `0-0` at the end of the
    /// list.
    ///
    /// ```
    /// use kevy_store::*;
    /// let mut s = Store::new();
    /// for ms in 1..=3 {
    ///     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    ///     s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(ms, 0)), f, MissingStream::Create, 0)?;
    /// }
    /// s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// s.xreadgroup(b"s", b"g", b"a", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// s.xdel(b"s", &[StreamId::new(1, 0)])?;
    /// // nothing is idle enough, but the deleted entry is found and dropped
    /// let (cursor, taken, gone) = s.xautoclaim(b"s", b"g", b"b", 1_000, StreamId::MIN, 1, ClaimMode::JustId, 100)?;
    /// assert_eq!((cursor, taken.len(), gone), (StreamId::new(2, 0), 0, vec![StreamId::new(1, 0)]));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    #[allow(clippy::too_many_arguments)]
    pub fn autoclaim(
        &mut self,
        group: &[u8],
        new_owner: &[u8],
        min_idle_ms: u64,
        start: StreamId,
        count: usize,
        mode: ClaimMode,
        now_ms: u64,
    ) -> Result<AutoclaimResult, StoreError> {
        let opts = XClaimOpts::default().with_mode(mode);
        let owner = SmallBytes::from_slice(new_owner);
        let Some(g) = self.groups.get_mut(group) else {
            return Err(StoreError::NoSuchKey);
        };
        super::ensure_consumer(g, &owner, now_ms);
        let mut attempts = count.saturating_mul(10);
        let (mut claimed, mut deleted) = (Vec::new(), Vec::new());
        let mut at = start;
        let mut next_cursor = StreamId::MIN;
        loop {
            let Some((&id, p)) = g.pel.range(at..).next() else { break };
            if attempts == 0 || claimed.len() + deleted.len() >= count {
                next_cursor = id;
                break;
            }
            attempts -= 1;
            let idle = now_ms.saturating_sub(p.delivery_time_ms);
            if !self.entries.contains_key(&id) {
                // claim_one drops a pending entry the stream lost
                claim_one(g, &self.entries, id, &owner, &opts, now_ms);
                deleted.push(id);
            } else if idle >= min_idle_ms && claim_one(g, &self.entries, id, &owner, &opts, now_ms)
            {
                claimed.push(id);
            }
            if id == StreamId::MAX {
                break;
            }
            at = id.next();
        }
        if let Some(cs) = g.consumers.get_mut(new_owner) {
            cs.last_seen_ms = now_ms;
            if !claimed.is_empty() {
                cs.last_active_ms = Some(now_ms);
            }
        }
        Ok(AutoclaimResult { next_cursor, claimed_ids: claimed, deleted_ids: deleted })
    }
}

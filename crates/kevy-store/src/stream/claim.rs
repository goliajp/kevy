//! `XCLAIM` / `XAUTOCLAIM` impls — split out of `stream/group.rs` so
//! both files stay under the project's ≤500-LOC rule. Owns the
//! `AutoclaimResult` return type alongside the methods that produce it.

use super::group::{ConsumerGroup, ensure_consumer};
#[path = "autoclaim.rs"]
mod autoclaim;
use super::{ClaimMode, EntryBatch, PelEntry, StreamData, StreamId};
use crate::StoreError;
#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::value::SmallBytes;

/// Snapshot of `XAUTOCLAIM` work in progress: cursor for the next
/// call, IDs successfully transferred, and IDs skipped because the
/// stream has since deleted them.
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
/// let mut stream = s.stream_view(b"s")?.unwrap().clone();
/// let r = stream.autoclaim(b"g", b"bob", 0, StreamId::MIN, 10, ClaimMode::Deliver, 200)?;
/// assert_eq!(r.claimed_ids, [StreamId::new(1, 0), StreamId::new(2, 0)]);
/// assert_eq!(r.next_cursor, StreamId::MIN, "the scan reached the end");
/// assert!(r.deleted_ids.is_empty());
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct AutoclaimResult {
    /// Where the next `XAUTOCLAIM` should resume: the id of the next
    /// pending entry, or `0-0` when the scan reached the end of the list.
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
    /// let mut stream = s.stream_view(b"s")?.unwrap().clone();
    /// // COUNT 1 stops after the first entry; the cursor is the next one
    /// let r = stream.autoclaim(b"g", b"bob", 0, StreamId::MIN, 1, ClaimMode::Deliver, 200)?;
    /// assert_eq!(r.next_cursor, StreamId::new(2, 0));
    /// let r = stream.autoclaim(b"g", b"bob", 0, r.next_cursor, 1, ClaimMode::Deliver, 200)?;
    /// assert_eq!(r.claimed_ids, [StreamId::new(2, 0)]);
    /// assert_eq!(r.next_cursor, StreamId::MIN, "nothing left to scan");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub next_cursor: StreamId,
    /// Entries transferred to the claiming consumer, in stream order.
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
    /// let mut stream = s.stream_view(b"s")?.unwrap().clone();
    /// // only entries idle for at least 150 ms move: none yet at t=200
    /// let r = stream.autoclaim(b"g", b"bob", 150, StreamId::MIN, 10, ClaimMode::Deliver, 200)?;
    /// assert!(r.claimed_ids.is_empty());
    /// let r = stream.autoclaim(b"g", b"bob", 150, StreamId::MIN, 10, ClaimMode::Deliver, 250)?;
    /// assert_eq!(r.claimed_ids.len(), 2);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub claimed_ids: Vec<StreamId>,
    /// Entries that were pending but no longer exist — deleted from the
    /// stream while a consumer still held them. XAUTOCLAIM drops them from
    /// the pending list and reports them here rather than claiming a
    /// message with no body.
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
    /// s.xdel(b"s", &[StreamId::new(1, 0)])?;
    /// let mut stream = s.stream_view(b"s")?.unwrap().clone();
    /// let r = stream.autoclaim(b"g", b"bob", 0, StreamId::MIN, 10, ClaimMode::Deliver, 200)?;
    /// assert_eq!(r.deleted_ids, [StreamId::new(1, 0)]);
    /// assert_eq!(r.claimed_ids, [StreamId::new(2, 0)]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub deleted_ids: Vec<StreamId>,
}

/// Knobs for `XCLAIM` ([`Store::xclaim`](crate::Store::xclaim)):
/// `min-idle-ms` plus the `IDLE`/`TIME`/`RETRYCOUNT`/`FORCE`/`JUSTID` flag
/// tail. The default is `XCLAIM`'s: no idle floor, no overrides, no
/// `FORCE`, a counted redelivery.
///
/// ```
/// use kevy_store::{ClaimMode, XClaimOpts};
/// let o = XClaimOpts::default().with_min_idle_ms(500).with_force(true).with_mode(ClaimMode::JustId);
/// assert_eq!((o.min_idle_ms, o.force, o.mode), (500, true, ClaimMode::JustId));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct XClaimOpts {
    /// Only claim entries idle for at least this many ms.
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
    /// // delivered at 100: at 150 the entry has been idle 50 ms
    /// let opts = XClaimOpts::default().with_min_idle_ms(60);
    /// assert!(s.xclaim(b"s", b"g", b"bob", &[StreamId::new(1, 0)], &opts, 150)?.is_empty());
    /// let opts = XClaimOpts::default().with_min_idle_ms(50);
    /// assert_eq!(s.xclaim(b"s", b"g", b"bob", &[StreamId::new(1, 0)], &opts, 150)?.len(), 1);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub min_idle_ms: u64,
    /// Override post-claim idle to this many ms (else 0 — XCLAIM resets
    /// the clock so the new owner has the full idle window).
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
    /// // the claimed entry reports 40 ms idle instead of starting from 0
    /// let opts = XClaimOpts::default().with_idle_ms(40);
    /// s.xclaim(b"s", b"g", b"bob", &[StreamId::new(1, 0)], &opts, 150)?;
    /// let rows = s.xpending_extended(b"s", b"g", None, StreamId::MIN, StreamId::MAX, 1, None, 150)?.unwrap().rows;
    /// assert_eq!(rows[0].idle_ms, 40);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub idle_override_ms: Option<u64>,
    /// Override post-claim delivery_time_ms to this absolute unix-ms.
    /// Takes precedence over `idle_override_ms` if both set. A time after
    /// the claim's own is taken as the claim's.
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
    /// let opts = XClaimOpts::default().with_time_ms(42);
    /// s.xclaim(b"s", b"g", b"bob", &[StreamId::new(1, 0)], &opts, 150)?;
    /// let g = s.stream_group_peek(b"s", b"g").unwrap();
    /// assert_eq!(g.pending_entry(StreamId::new(1, 0)).unwrap().delivery_time_ms, 42);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub time_override_ms: Option<u64>,
    /// Override post-claim `delivery_count` (else +=1).
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
    /// let opts = XClaimOpts::default().with_retrycount(7);
    /// s.xclaim(b"s", b"g", b"bob", &[StreamId::new(1, 0)], &opts, 150)?;
    /// let g = s.stream_group_peek(b"s", b"g").unwrap();
    /// assert_eq!(g.pending_entry(StreamId::new(1, 0)).unwrap().delivery_count, 7);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub retrycount_override: Option<u64>,
    /// `FORCE`: claim even if the entry isn't in the PEL yet (creates
    /// a fresh PEL row with delivery_count=1).
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
    /// let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// let id = s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, 3)?.unwrap();
    /// // never delivered, so not pending: only FORCE claims it
    /// let plain = XClaimOpts::default();
    /// assert!(s.xclaim(b"s", b"g", b"bob", &[id], &plain, 150)?.is_empty());
    /// let forced = XClaimOpts::default().with_force(true);
    /// assert_eq!(s.xclaim(b"s", b"g", b"bob", &[id], &forced, 150)?.len(), 1);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub force: bool,
    /// `JUSTID` or a counted redelivery.
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
    /// let opts = XClaimOpts::default().with_mode(ClaimMode::JustId);
    /// s.xclaim(b"s", b"g", b"bob", &[StreamId::new(1, 0)], &opts, 150)?;
    /// let e = s.stream_group_peek(b"s", b"g").unwrap().pending_entry(StreamId::new(1, 0)).unwrap();
    /// assert_eq!((e.consumer.as_slice(), e.delivery_count), (b"bob".as_slice(), 1));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub mode: ClaimMode,
    /// `LASTID id`: move the group's last-delivered ID up to `id`, when
    /// it is behind.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// # s.xadd(b"s", XAddIdSpec::Explicit(StreamId::new(1, 0)), f, MissingStream::Create, 0)?;
    /// s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// let opts = XClaimOpts::default().with_last_id(StreamId::new(7, 0));
    /// s.xclaim(b"s", b"g", b"bob", &[], &opts, 150)?;
    /// assert_eq!(s.stream_group_peek(b"s", b"g").unwrap().last_delivered_id(), StreamId::new(7, 0));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub last_id: Option<StreamId>,
}

impl XClaimOpts {
    /// Claim only entries idle for at least `ms`.
    ///
    /// ```
    /// assert_eq!(kevy_store::XClaimOpts::default().with_min_idle_ms(9).min_idle_ms, 9);
    /// ```
    #[must_use]
    pub fn with_min_idle_ms(mut self, ms: u64) -> Self {
        self.min_idle_ms = ms;
        self
    }

    /// `IDLE ms`: the claimed entry's idle time afterwards.
    ///
    /// ```
    /// assert_eq!(kevy_store::XClaimOpts::default().with_idle_ms(5).idle_override_ms, Some(5));
    /// ```
    #[must_use]
    pub fn with_idle_ms(mut self, ms: u64) -> Self {
        self.idle_override_ms = Some(ms);
        self
    }

    /// `TIME unix-ms`: the claimed entry's delivery time afterwards.
    ///
    /// ```
    /// assert_eq!(kevy_store::XClaimOpts::default().with_time_ms(7).time_override_ms, Some(7));
    /// ```
    #[must_use]
    pub fn with_time_ms(mut self, unix_ms: u64) -> Self {
        self.time_override_ms = Some(unix_ms);
        self
    }

    /// `RETRYCOUNT n`: the claimed entry's delivery count afterwards.
    ///
    /// ```
    /// assert_eq!(kevy_store::XClaimOpts::default().with_retrycount(3).retrycount_override, Some(3));
    /// ```
    #[must_use]
    pub fn with_retrycount(mut self, n: u64) -> Self {
        self.retrycount_override = Some(n);
        self
    }

    /// `LASTID id`: see [`XClaimOpts::last_id`].
    ///
    /// ```
    /// use kevy_store::{StreamId, XClaimOpts};
    /// let o = XClaimOpts::default().with_last_id(StreamId::new(3, 0));
    /// assert_eq!(o.last_id, Some(StreamId::new(3, 0)));
    /// ```
    #[must_use]
    pub fn with_last_id(mut self, id: StreamId) -> Self {
        self.last_id = Some(id);
        self
    }

    /// `FORCE`: claim IDs that are not pending yet.
    ///
    /// ```
    /// assert!(kevy_store::XClaimOpts::default().with_force(true).force);
    /// ```
    #[must_use]
    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// `JUSTID`, or a counted redelivery.
    ///
    /// ```
    /// use kevy_store::{ClaimMode, XClaimOpts};
    /// assert_eq!(XClaimOpts::default().with_mode(ClaimMode::JustId).mode, ClaimMode::JustId);
    /// ```
    #[must_use]
    pub fn with_mode(mut self, mode: ClaimMode) -> Self {
        self.mode = mode;
        self
    }
}

impl StreamData {
    /// `XCLAIM key group consumer min-idle-ms id [id ...] [...]`.
    /// Returns the IDs successfully claimed (the dispatcher decides
    /// whether to emit JUSTID or full entries).
    pub fn claim(
        &mut self,
        group: &[u8],
        new_owner: &[u8],
        ids: &[StreamId],
        opts: &XClaimOpts,
        now_ms: u64,
    ) -> Result<Vec<StreamId>, StoreError> {
        let Some(g) = self.groups.get_mut(group) else {
            return Err(StoreError::NoSuchKey);
        };
        let new_owner_smb = SmallBytes::from_slice(new_owner);
        ensure_consumer(g, &new_owner_smb, now_ms);
        let mut claimed = Vec::new();
        for id in ids {
            if !claim_one(g, &self.entries, *id, &new_owner_smb, opts, now_ms) {
                continue;
            }
            claimed.push(*id);
        }
        if let Some(last) = opts.last_id
            && last > g.last_delivered_id
        {
            g.last_delivered_id = last;
        }
        // a claim is the consumer's contact whatever it takes, and makes it
        // active when it takes something
        if let Some(cs) = g.consumers.get_mut(new_owner) {
            cs.last_seen_ms = now_ms;
            if !claimed.is_empty() {
                cs.last_active_ms = Some(now_ms);
            }
        }
        Ok(claimed)
    }

    /// Field-value payload list pairing with `ids` (from
    /// [`Self::claim`] / [`Self::autoclaim`]). Skips IDs that were
    /// XDELed between claim and emit.
    pub fn payloads_for(&self, ids: &[StreamId]) -> EntryBatch {
        ids.iter()
            .filter_map(|id| {
                self.entries
                    .get(id)
                    .map(|fv| (*id, fv.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect()))
            })
            .collect()
    }
}

/// Attempt one XCLAIM. Returns `true` if the entry was successfully
/// transferred to `new_owner`. The `entries` ref is the stream's
/// entry map (passed in to avoid an extra `&mut self` borrow when
/// `claim` is called over a slice of IDs).
pub(super) fn claim_one(
    g: &mut ConsumerGroup,
    entries: &alloc::collections::BTreeMap<StreamId, Vec<(SmallBytes, SmallBytes)>>,
    id: StreamId,
    new_owner: &SmallBytes,
    opts: &XClaimOpts,
    now_ms: u64,
) -> bool {
    let entry_present = g.pel.contains_key(&id);
    if !entry_present && !opts.force {
        return false;
    }
    if !entries.contains_key(&id) {
        if let Some(p) = g.pel.remove(&id)
            && let Some(cs) = g.consumers.get_mut(p.consumer.as_slice())
        {
            cs.pel_count = cs.pel_count.saturating_sub(1);
        }
        return false;
    }
    if let Some(existing) = g.pel.get(&id) {
        let idle = now_ms.saturating_sub(existing.delivery_time_ms);
        if idle < opts.min_idle_ms {
            return false;
        }
    }
    // a time past the claim's own, or an idle time reaching before the
    // epoch, is the claim's own time
    let new_dt = match (opts.time_override_ms, opts.idle_override_ms) {
        (Some(t), _) => Some(t),
        (None, Some(i)) => now_ms.checked_sub(i),
        (None, None) => None,
    }
    .filter(|t| *t <= now_ms)
    .unwrap_or(now_ms);
    let new_dc = opts.retrycount_override.unwrap_or_else(|| {
        // a row FORCE makes counts as delivered once already
        let base = g.pel.get(&id).map_or(1, |p| p.delivery_count);
        match opts.mode {
            ClaimMode::JustId => base.max(1),
            ClaimMode::Deliver => super::group::delivered_again(base),
        }
    });
    let prev = g.pel.insert(
        id,
        PelEntry { consumer: new_owner.clone(), delivery_time_ms: new_dt, delivery_count: new_dc },
    );
    transfer_ownership_counts(g, prev.as_ref(), new_owner);
    true
}

pub(super) fn transfer_ownership_counts(
    g: &mut ConsumerGroup,
    prev: Option<&PelEntry>,
    new_owner: &SmallBytes,
) {
    match prev {
        Some(p) if p.consumer != *new_owner => {
            if let Some(cs) = g.consumers.get_mut(p.consumer.as_slice()) {
                cs.pel_count = cs.pel_count.saturating_sub(1);
            }
            if let Some(cs) = g.consumers.get_mut(new_owner.as_slice()) {
                cs.pel_count = cs.pel_count.saturating_add(1);
            }
        }
        Some(_) => {}
        None => {
            if let Some(cs) = g.consumers.get_mut(new_owner.as_slice()) {
                cs.pel_count = cs.pel_count.saturating_add(1);
            }
        }
    }
}

//! `XCLAIM` / `XAUTOCLAIM` impls — split out of `stream/group.rs` so
//! both files stay under the project's ≤500-LOC rule. Owns the
//! `AutoclaimResult` return type alongside the methods that produce it.

use super::group::{ConsumerGroup, ensure_consumer};
use super::{ClaimMode, EntryBatch, PelEntry, StreamData, StreamId};
use crate::StoreError;
#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::value::SmallBytes;

/// Snapshot of `XAUTOCLAIM` work in progress: cursor for the next
/// call, IDs successfully transferred, and IDs skipped because the
/// stream has since deleted them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct AutoclaimResult {
    /// Where the next `XAUTOCLAIM` should resume. `0-0` when the scan
    /// reached the end of the pending list.
    pub next_cursor: StreamId,
    /// Entries transferred to the claiming consumer, in stream order.
    pub claimed_ids: Vec<StreamId>,
    /// Entries that were pending but no longer exist — deleted from the
    /// stream while a consumer still held them. XAUTOCLAIM drops them from
    /// the pending list and reports them here rather than claiming a
    /// message with no body.
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
    pub min_idle_ms: u64,
    /// Override post-claim idle to this many ms (else 0 — XCLAIM resets
    /// the clock so the new owner has the full idle window).
    pub idle_override_ms: Option<u64>,
    /// Override post-claim delivery_time_ms to this absolute unix-ms.
    /// Takes precedence over `idle_override_ms` if both set.
    pub time_override_ms: Option<u64>,
    /// Override post-claim `delivery_count` (else +=1).
    pub retrycount_override: Option<u32>,
    /// `FORCE`: claim even if the entry isn't in the PEL yet (creates
    /// a fresh PEL row with delivery_count=1).
    pub force: bool,
    /// `JUSTID` or a counted redelivery.
    pub mode: ClaimMode,
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
    pub fn with_retrycount(mut self, n: u32) -> Self {
        self.retrycount_override = Some(n);
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
        Ok(claimed)
    }

    /// `XAUTOCLAIM key group consumer min-idle-ms start [COUNT n]
    /// [JUSTID]`. Walks the PEL from `start` onward, claiming the
    /// first `count` entries whose idle ≥ `min_idle_ms`. Returns
    /// `(next_cursor_id, claimed_ids, deleted_ids)`.
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
        let opts = XClaimOpts::default().with_min_idle_ms(min_idle_ms).with_mode(mode);
        let candidates: Vec<StreamId> = {
            let Some(g) = self.groups.get(group) else {
                return Err(StoreError::NoSuchKey);
            };
            g.pel
                .range(start..=StreamId::MAX)
                .filter(|(_, p)| now_ms.saturating_sub(p.delivery_time_ms) >= min_idle_ms)
                .take(count)
                .map(|(id, _)| *id)
                .collect()
        };
        let next_cursor = candidates.last().map_or(StreamId::MIN, |id| id.next());
        let claimed = self.claim(group, new_owner, &candidates, &opts, now_ms)?;
        let mut deleted = Vec::new();
        for id in &candidates {
            if !self.entries.contains_key(id) && !claimed.contains(id) {
                deleted.push(*id);
            }
        }
        Ok(AutoclaimResult { next_cursor, claimed_ids: claimed, deleted_ids: deleted })
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
fn claim_one(
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
    let new_dt = opts
        .time_override_ms
        .or_else(|| opts.idle_override_ms.map(|i| now_ms.saturating_sub(i)))
        .unwrap_or(now_ms);
    let new_dc = opts.retrycount_override.unwrap_or_else(|| {
        let base = g.pel.get(&id).map_or(0, |p| p.delivery_count);
        match opts.mode {
            ClaimMode::JustId => base.max(1),
            ClaimMode::Deliver => base.saturating_add(1),
        }
    });
    let prev = g.pel.insert(
        id,
        PelEntry { consumer: new_owner.clone(), delivery_time_ms: new_dt, delivery_count: new_dc },
    );
    transfer_ownership_counts(g, prev.as_ref(), new_owner);
    true
}

fn transfer_ownership_counts(
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

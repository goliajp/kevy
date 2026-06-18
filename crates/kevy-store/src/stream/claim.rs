//! `XCLAIM` / `XAUTOCLAIM` impls — split out of `stream/group.rs` so
//! both files stay under the project's ≤500-LOC rule. Owns the
//! `AutoclaimResult` return type alongside the methods that produce it.

use super::group::{ConsumerGroup, ensure_consumer};
use super::{EntryBatch, PelEntry, StreamData, StreamId, XClaimOpts};
use crate::value::SmallBytes;
use crate::StoreError;

/// Snapshot of `XAUTOCLAIM` work in progress: cursor for the next
/// call, IDs successfully transferred, and IDs skipped because the
/// stream has since deleted them.
pub struct AutoclaimResult {
    pub next_cursor: StreamId,
    pub claimed_ids: Vec<StreamId>,
    pub deleted_ids: Vec<StreamId>,
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
        justid: bool,
        now_ms: u64,
    ) -> Result<AutoclaimResult, StoreError> {
        let opts = XClaimOpts {
            min_idle_ms,
            idle_override_ms: None,
            time_override_ms: None,
            retrycount_override: None,
            force: false,
            justid,
        };
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
        let next_cursor = candidates
            .last()
            .map_or(StreamId::MIN, |id| id.next());
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
                self.entries.get(id).map(|fv| {
                    (
                        *id,
                        fv.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect(),
                    )
                })
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
    entries: &std::collections::BTreeMap<StreamId, Vec<(SmallBytes, SmallBytes)>>,
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
        if opts.justid { base.max(1) } else { base.saturating_add(1) }
    });
    let prev = g.pel.insert(
        id,
        PelEntry {
            consumer: new_owner.clone(),
            delivery_time_ms: new_dt,
            delivery_count: new_dc,
        },
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

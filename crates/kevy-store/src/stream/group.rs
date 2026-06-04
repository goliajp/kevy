//! Consumer groups for v2-7 streams (sprint B). The group state lives
//! inside its parent [`crate::stream::StreamData`] so XADD / XDEL can
//! see the group map without an extra lookup. This file owns the
//! types + the in-stream operations; the `Store`-side wrappers live in
//! `stream/store.rs` next to the rest of the public API.

use std::collections::BTreeMap;

use kevy_map::KevyMap;

use super::{EntryBatch, StreamData, StreamId};
pub(super) use super::claim::AutoclaimResult;
use crate::value::SmallBytes;
use crate::StoreError;

/// One consumer group's state. Sorted PEL plus a map of known
/// consumers (with cached pel_count for O(1) XINFO answers).
pub struct ConsumerGroup {
    /// Highest ID delivered to any consumer in this group. Bumped by
    /// XREADGROUP with `>`; settable via XGROUP SETID.
    pub last_delivered_id: StreamId,
    /// Pending-Entries List: every ID delivered but not yet ACKed.
    /// Sorted by ID for `XPENDING start end` range queries.
    pub pel: BTreeMap<StreamId, PelEntry>,
    /// Consumers known to this group (by name).
    pub consumers: KevyMap<SmallBytes, Box<ConsumerState>>,
}

impl Default for ConsumerGroup {
    fn default() -> Self {
        Self {
            last_delivered_id: StreamId::MIN,
            pel: BTreeMap::new(),
            consumers: KevyMap::default(),
        }
    }
}

/// One pending entry: who got it, when, and how many times.
#[derive(Clone, Debug)]
pub struct PelEntry {
    /// Owning consumer's name. Used by XPENDING's `consumer` filter
    /// and XCLAIM's ownership transfer.
    pub consumer: SmallBytes,
    /// Last delivery wall-clock (unix-ms). XCLAIM compares idle =
    /// `now - delivery_time_ms` against its `min-idle-ms` arg.
    pub delivery_time_ms: u64,
    /// Number of times this entry has been delivered (=1 on first
    /// XREADGROUP, +=1 on each XCLAIM that doesn't have JUSTID).
    pub delivery_count: u32,
}

/// Per-consumer cached counters so `XINFO CONSUMERS` answers in O(1).
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ConsumerState {
    /// Consumer name. Read by XINFO CONSUMERS (sprint C).
    pub name: SmallBytes,
    /// Last wall-clock (unix-ms) the consumer interacted with the
    /// group (any XREADGROUP / XACK / XCLAIM touch).
    pub last_seen_ms: u64,
    /// Cached size of this consumer's slice of the PEL.
    pub pel_count: usize,
}

/// `XGROUP CREATE` ID argument: either an explicit ID or `$`
/// (= current stream's `last_id`, resolved by the caller).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupCreateMode {
    /// `<ms>-<seq>` literal — the group's `last_delivered_id` starts here.
    AtId(StreamId),
    /// `$` — resolve to the stream's current `last_id` at create time.
    AtCurrent,
}

/// Summary form of `XPENDING key group` (only 3 args): total pending,
/// min/max IDs across the PEL, and per-consumer aggregate counts.
pub struct PendingSummary {
    /// Total pending entries across all consumers.
    pub total: u64,
    /// Smallest and largest pending IDs, or `None` if the PEL is empty.
    pub id_range: Option<(StreamId, StreamId)>,
    /// `(consumer, count)` pairs in arbitrary order.
    pub by_consumer: Vec<(Vec<u8>, u64)>,
}

/// Extended form of `XPENDING key group [IDLE ms] start end count
/// [consumer]`: one row per matching PEL entry.
pub struct PendingExtended {
    /// Per-entry rows in ID-ascending order.
    pub rows: Vec<PendingExtendedRow>,
}

/// One row of the extended XPENDING reply.
pub struct PendingExtendedRow {
    /// Entry ID.
    pub id: StreamId,
    /// Owning consumer's name.
    pub consumer: Vec<u8>,
    /// Idle time in milliseconds (now - delivery_time_ms).
    pub idle_ms: u64,
    /// Delivery count.
    pub delivery_count: u32,
}

/// Knobs for [`StreamData::xclaim`]: `min-idle-ms` plus the
/// `IDLE`/`TIME`/`RETRYCOUNT`/`FORCE`/`JUSTID` flag tail.
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
    /// `JUSTID`: skip the +=1 on `delivery_count` (used by tools that
    /// don't intend a real redelivery).
    pub justid: bool,
}

impl StreamData {
    /// `XGROUP CREATE key group <id|$> [MKSTREAM]`. Returns `true` if
    /// a new group was created; `false` if the group already existed
    /// (caller should report Redis's `-BUSYGROUP` error in that case).
    pub fn group_create(
        &mut self,
        name: &[u8],
        mode: GroupCreateMode,
    ) -> Result<bool, StoreError> {
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
            }),
        );
        Ok(true)
    }

    /// `XGROUP DESTROY key group`. Returns `true` if a group was dropped.
    pub fn group_destroy(&mut self, name: &[u8]) -> bool {
        self.groups.remove(name).is_some()
    }

    /// `XGROUP SETID key group <id|$>`. Returns `false` if the group
    /// doesn't exist.
    pub fn group_setid(&mut self, name: &[u8], mode: GroupCreateMode) -> bool {
        let Some(g) = self.groups.get_mut(name) else {
            return false;
        };
        g.last_delivered_id = match mode {
            GroupCreateMode::AtId(id) => id,
            GroupCreateMode::AtCurrent => self.last_id,
        };
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
        g.consumers.insert(
            SmallBytes::from_slice(consumer),
            Box::new(ConsumerState {
                name: SmallBytes::from_slice(consumer),
                last_seen_ms: now_ms,
                pel_count: 0,
            }),
        );
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
        noack: bool,
        now_ms: u64,
    ) -> Result<EntryBatch, StoreError> {
        let Some(g) = self.groups.get_mut(group) else {
            return Err(StoreError::NoSuchKey);
        };
        let consumer_smb = SmallBytes::from_slice(consumer);
        ensure_consumer(g, &consumer_smb, now_ms);
        if let Some(cs) = g.consumers.get_mut(consumer_smb.as_slice()) {
            cs.last_seen_ms = now_ms;
        }
        match last_seen_arg {
            ReadGroupId::New => {
                let start = g.last_delivered_id.next();
                let entries: Vec<(StreamId, &[(SmallBytes, SmallBytes)])> = self
                    .entries
                    .range(start..=StreamId::MAX)
                    .map(|(id, fv)| (*id, fv.as_slice()))
                    .collect();
                let take = match count {
                    Some(n) => entries.into_iter().take(n).collect::<Vec<_>>(),
                    None => entries,
                };
                if take.is_empty() {
                    return Ok(Vec::new());
                }
                if !noack {
                    record_deliveries(g, &consumer_smb, &take, now_ms);
                }
                let g_mut = self.groups.get_mut(group).expect("present");
                if let Some((last_id, _)) = take.last() {
                    g_mut.last_delivered_id = *last_id;
                }
                Ok(super::clone_entries(take))
            }
            ReadGroupId::ReplayAfter(after) => {
                let mut hit: Vec<(StreamId, Vec<(SmallBytes, SmallBytes)>)> = Vec::new();
                let consumer_match = consumer_smb.clone();
                for (id, pel_entry) in g.pel.range(after.next()..=StreamId::MAX) {
                    if pel_entry.consumer != consumer_match {
                        continue;
                    }
                    if let Some(fv) = self.entries.get(id) {
                        hit.push((*id, fv.clone()));
                    }
                    if let Some(n) = count
                        && hit.len() >= n
                    {
                        break;
                    }
                }
                Ok(hit
                    .into_iter()
                    .map(|(id, fv)| {
                        (
                            id,
                            fv.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect(),
                        )
                    })
                    .collect())
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
            if let Some((_, n)) = counts.iter_mut().find(|(name, _)| name == p.consumer.as_slice()) {
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

/// XREADGROUP's per-stream ID: either `>` (= new entries) or an explicit
/// "after this id" for PEL replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadGroupId {
    /// `>` — new entries only.
    New,
    /// `<id>` — replay PEL entries strictly after this id.
    ReplayAfter(StreamId),
}

/// Idempotent insert: ensure the named consumer exists in this group's
/// roster so subsequent `pel_count`/`last_seen_ms` updates have a slot.
pub(super) fn ensure_consumer(g: &mut ConsumerGroup, name: &SmallBytes, now_ms: u64) {
    if g.consumers.get(name.as_slice()).is_none() {
        g.consumers.insert(
            name.clone(),
            Box::new(ConsumerState {
                name: name.clone(),
                last_seen_ms: now_ms,
                pel_count: 0,
            }),
        );
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
            PelEntry {
                consumer: consumer.clone(),
                delivery_time_ms: now_ms,
                delivery_count: 0,
            }
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

//! `XPENDING`: the pending-list summary and its per-entry form. Split
//! from `group.rs` to keep it under the 500-LOC cap.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;

use super::{StreamData, StreamId};

/// Summary form of `XPENDING key group` (only 3 args): total pending,
/// min/max IDs across the PEL, and per-consumer aggregate counts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PendingExtended {
    /// Per-entry rows in ID-ascending order.
    pub rows: Vec<PendingExtendedRow>,
}

/// One row of the extended XPENDING reply.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
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

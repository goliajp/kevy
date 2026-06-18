//! Per-replica slot bookkeeping for the primary's streaming loop.
//!
//! Each connected (or recently-disconnected) replica owns one
//! [`ReplicaSlot`]. The [`SlotTable`] is the source of truth for "who
//! still has a chance to resume from the backlog vs whom we have
//! given up on". Slots are pure data; socket lifetime and the
//! streaming loop's wakeups live in the wiring layer.
//!
//! Clock policy: all timestamps are `u64` monotonic nanoseconds
//! supplied by the caller. The module never reads the clock itself —
//! tests pump synthetic timestamps, and the production hook uses a
//! single `Instant`-derived `u64`. Same pattern as the cached-clock
//! work in `kevy-store`.

/// One connected-or-recently-disconnected replica.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaSlot {
    /// Operator-set replica identifier (opaque to the primary other
    /// than for slot bookkeeping).
    pub id: String,
    /// Monotonic ns timestamp of the most recent contact (handshake
    /// or ack). Drives expiry under `reconnect_window_ms`.
    pub last_seen_ns: u64,
    /// Highest offset the replica has acked. The streaming loop
    /// resumes sending from here on reconnect.
    pub acked_offset: u64,
}

/// Mutable collection of [`ReplicaSlot`]s. Slots are addressed by id;
/// duplicate insertion upserts (newer state wins). Replica counts in
/// realistic deployments are small (< 16); a linear `Vec` is faster
/// than a `HashMap` at this size and avoids the cost of the hasher
/// the rest of the workspace uses.
#[derive(Debug, Default)]
pub struct SlotTable {
    slots: Vec<ReplicaSlot>,
}

impl SlotTable {
    /// A fresh empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of slots currently tracked.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the table has no slots.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Look up a slot by id.
    pub fn get(&self, id: &str) -> Option<&ReplicaSlot> {
        self.slots.iter().find(|s| s.id == id)
    }

    /// Iterate over all slots.
    pub fn iter(&self) -> impl Iterator<Item = &ReplicaSlot> {
        self.slots.iter()
    }

    /// Insert a new slot or update an existing one. Touching always
    /// refreshes `last_seen_ns` and advances `acked_offset` if the
    /// new value is higher (a slot's acked offset is monotonic — a
    /// peer reporting a lower offset than we already recorded is
    /// almost always a bug; the silent max() here defends the
    /// invariant).
    pub fn insert_or_touch(&mut self, id: &str, acked_offset: u64, now_ns: u64) {
        if let Some(s) = self.slots.iter_mut().find(|s| s.id == id) {
            s.last_seen_ns = now_ns;
            if acked_offset > s.acked_offset {
                s.acked_offset = acked_offset;
            }
            return;
        }
        self.slots.push(ReplicaSlot {
            id: id.to_string(),
            last_seen_ns: now_ns,
            acked_offset,
        });
    }

    /// Remove the slot with the given id. Returns `true` if a slot
    /// was actually removed.
    pub fn remove(&mut self, id: &str) -> bool {
        if let Some(pos) = self.slots.iter().position(|s| s.id == id) {
            self.slots.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Drop slots whose `last_seen_ns + window_ns ≤ now_ns`. Returns
    /// the ids of the dropped slots so callers can fire metrics or
    /// log lines. Order is unspecified (swap-remove internally).
    pub fn expire(&mut self, now_ns: u64, window_ns: u64) -> Vec<String> {
        let mut dropped = Vec::new();
        // Walk backward so swap_remove doesn't shift indices we still
        // need to visit.
        let mut i = self.slots.len();
        while i > 0 {
            i -= 1;
            let cutoff = self.slots[i].last_seen_ns.saturating_add(window_ns);
            if cutoff <= now_ns {
                let s = self.slots.swap_remove(i);
                dropped.push(s.id);
            }
        }
        dropped
    }

    /// Lowest acked offset across all tracked slots. Useful for the
    /// streaming loop to know how far back the backlog must still
    /// retain frames; `None` when the table is empty.
    pub fn min_acked_offset(&self) -> Option<u64> {
        self.slots.iter().map(|s| s.acked_offset).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_table_is_empty() {
        let t = SlotTable::new();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert_eq!(t.min_acked_offset(), None);
    }

    #[test]
    fn insert_then_get_returns_the_slot() {
        let mut t = SlotTable::new();
        t.insert_or_touch("a", 5, 100);
        let s = t.get("a").unwrap();
        assert_eq!(s.id, "a");
        assert_eq!(s.acked_offset, 5);
        assert_eq!(s.last_seen_ns, 100);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn touch_advances_last_seen_and_acked_offset() {
        let mut t = SlotTable::new();
        t.insert_or_touch("a", 5, 100);
        t.insert_or_touch("a", 9, 200);
        let s = t.get("a").unwrap();
        assert_eq!(s.acked_offset, 9);
        assert_eq!(s.last_seen_ns, 200);
        // No duplicate row.
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn touch_with_lower_acked_offset_keeps_the_higher_one() {
        let mut t = SlotTable::new();
        t.insert_or_touch("a", 10, 100);
        // Peer reports a lower offset (bug / stale ack); slot keeps 10.
        t.insert_or_touch("a", 7, 200);
        let s = t.get("a").unwrap();
        assert_eq!(s.acked_offset, 10);
        assert_eq!(s.last_seen_ns, 200, "last_seen still advances");
    }

    #[test]
    fn remove_existing_returns_true_and_drops_slot() {
        let mut t = SlotTable::new();
        t.insert_or_touch("a", 1, 100);
        assert!(t.remove("a"));
        assert!(t.is_empty());
        assert_eq!(t.get("a"), None);
    }

    #[test]
    fn remove_missing_returns_false() {
        let mut t = SlotTable::new();
        assert!(!t.remove("missing"));
    }

    #[test]
    fn expire_drops_slots_past_window() {
        let mut t = SlotTable::new();
        t.insert_or_touch("old", 1, 100);
        t.insert_or_touch("fresh", 1, 500);
        // Window 200 ns; "now" = 350. "old" expires (100+200 ≤ 350),
        // "fresh" survives (500+200 > 350).
        let dropped = t.expire(350, 200);
        assert_eq!(dropped, vec!["old".to_string()]);
        assert_eq!(t.len(), 1);
        assert!(t.get("fresh").is_some());
    }

    #[test]
    fn expire_when_nothing_expires_returns_empty() {
        let mut t = SlotTable::new();
        t.insert_or_touch("a", 1, 1000);
        let dropped = t.expire(1100, 500); // 1000 + 500 = 1500 > 1100
        assert!(dropped.is_empty());
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn expire_with_overflow_window_saturates_does_not_panic() {
        // `last_seen + window` must saturate at u64::MAX rather than
        // wrap. With now=u64::MAX-1 the saturated cutoff stays above
        // now, so the slot survives. (At now=u64::MAX a saturated
        // cutoff would equal now and the slot does expire — but the
        // safety invariant being asserted is "no panic".)
        let mut t = SlotTable::new();
        t.insert_or_touch("a", 1, u64::MAX - 10);
        let dropped = t.expire(u64::MAX - 1, u64::MAX);
        assert!(dropped.is_empty());
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn min_acked_offset_returns_the_floor() {
        let mut t = SlotTable::new();
        t.insert_or_touch("a", 7, 100);
        t.insert_or_touch("b", 3, 100);
        t.insert_or_touch("c", 12, 100);
        assert_eq!(t.min_acked_offset(), Some(3));
    }

    #[test]
    fn iter_visits_every_slot() {
        let mut t = SlotTable::new();
        t.insert_or_touch("a", 1, 100);
        t.insert_or_touch("b", 2, 100);
        let mut ids: Vec<_> = t.iter().map(|s| s.id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn expire_all_when_window_zero() {
        let mut t = SlotTable::new();
        t.insert_or_touch("a", 1, 100);
        t.insert_or_touch("b", 1, 100);
        let dropped = t.expire(100, 0);
        assert_eq!(dropped.len(), 2);
        assert!(t.is_empty());
    }
}

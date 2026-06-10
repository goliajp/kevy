//! Consumer-group exchange types + the `XSETID` scalar setter — the
//! pieces persistence (snapshot v4, AOF rewrite, reshard's `load_value`
//! redistribution) needs to carry group/PEL state across a dump/load
//! boundary. Split from `stream/mod.rs` to stay under the 500-LOC cap.

use std::collections::BTreeMap;

use kevy_map::KevyMap;

use super::group::{ConsumerGroup, ConsumerState, PelEntry};
use super::{StreamData, StreamId};
use crate::StoreError;
use crate::value::SmallBytes;

/// One PEL row in primitive form: `(ms, seq, consumer, delivery_time_ms,
/// delivery_count)`. The persist crate serializes these verbatim.
pub type LoadedPelEntry = (u64, u64, Vec<u8>, u64, u32);

/// One consumer group decoded into primitive tuples — the dump/load wire
/// form shared by snapshot v4, AOF-rewrite filtering, and reshard's
/// in-memory redistribution.
pub struct LoadedGroup {
    /// Group name.
    pub name: Vec<u8>,
    /// `last_delivered_id` as `(ms, seq)`.
    pub last_delivered: (u64, u64),
    /// `(name, last_seen_ms)` per known consumer. `pel_count` is
    /// recomputed from `pel` on import.
    pub consumers: Vec<(Vec<u8>, u64)>,
    /// Every PEL row, including tombstones (entries XDEL'd while
    /// pending) — snapshot keeps those; AOF rewrite filters them.
    pub pel: Vec<LoadedPelEntry>,
}

impl StreamData {
    /// Does an entry with `id` currently exist? AOF rewrite uses this
    /// to filter tombstone PEL rows (XCLAIM can't re-create those).
    pub fn contains_entry(&self, id: StreamId) -> bool {
        self.entries.contains_key(&id)
    }

    /// Dump every group into the primitive exchange form.
    pub fn export_groups(&self) -> Vec<LoadedGroup> {
        self.groups
            .iter()
            .map(|(name, g)| LoadedGroup {
                name: name.to_vec(),
                last_delivered: (g.last_delivered_id.ms, g.last_delivered_id.seq),
                consumers: g
                    .consumers
                    .iter()
                    .map(|(c, cs)| (c.to_vec(), cs.last_seen_ms))
                    .collect(),
                pel: g
                    .pel
                    .iter()
                    .map(|(id, p)| {
                        (id.ms, id.seq, p.consumer.to_vec(), p.delivery_time_ms, p.delivery_count)
                    })
                    .collect(),
            })
            .collect()
    }

    /// Rebuild the group map from the exchange form (loader-side twin of
    /// [`Self::export_groups`]). Per-consumer `pel_count` is recomputed;
    /// a PEL owner missing from the consumer roster (hand-built or
    /// corrupt file) gets a roster slot rather than a panic.
    pub fn import_groups(&mut self, groups: Vec<LoadedGroup>) {
        for lg in groups {
            let mut consumers: KevyMap<SmallBytes, Box<ConsumerState>> = KevyMap::default();
            for (name, last_seen_ms) in lg.consumers {
                let name = SmallBytes::from_vec(name);
                consumers.insert(
                    name.clone(),
                    Box::new(ConsumerState { name, last_seen_ms, pel_count: 0 }),
                );
            }
            let mut pel: BTreeMap<StreamId, PelEntry> = BTreeMap::new();
            for (ms, seq, consumer, delivery_time_ms, delivery_count) in lg.pel {
                let consumer = SmallBytes::from_vec(consumer);
                if consumers.get(consumer.as_slice()).is_none() {
                    consumers.insert(
                        consumer.clone(),
                        Box::new(ConsumerState {
                            name: consumer.clone(),
                            last_seen_ms: 0,
                            pel_count: 0,
                        }),
                    );
                }
                if let Some(cs) = consumers.get_mut(consumer.as_slice()) {
                    cs.pel_count += 1;
                }
                pel.insert(StreamId { ms, seq }, PelEntry {
                    consumer,
                    delivery_time_ms,
                    delivery_count,
                });
            }
            self.groups.insert(
                SmallBytes::from_vec(lg.name),
                Box::new(ConsumerGroup {
                    last_delivered_id: StreamId {
                        ms: lg.last_delivered.0,
                        seq: lg.last_delivered.1,
                    },
                    pel,
                    consumers,
                }),
            );
        }
    }

    /// `XSETID key last-id [ENTRIESADDED n] [MAXDELETEDID id]` — overwrite
    /// the stream's scalar state. Rejects a `last_id` below the current
    /// top entry (Redis: "smaller than the target stream top item").
    pub fn xsetid(
        &mut self,
        last_id: StreamId,
        entries_added: Option<u64>,
        max_deleted_id: Option<StreamId>,
    ) -> Result<(), StoreError> {
        if let Some((top, _)) = self.entries.iter().next_back()
            && last_id < *top
        {
            return Err(StoreError::OutOfRange);
        }
        self.last_id = last_id;
        if let Some(n) = entries_added {
            self.entries_added = n;
        }
        if let Some(id) = max_deleted_id {
            self.max_deleted_id = id;
        }
        Ok(())
    }
}

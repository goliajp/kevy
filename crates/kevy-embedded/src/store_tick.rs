//! `Store::tick` — the Manual-mode maintenance cadence, the same
//! per-shard body the background reaper drives. A `#[path]` child of
//! `store.rs`, split under the 500-LOC rule.

use kevy_store::ExpireStats;

use crate::store::{Store, lock_write};

impl Store {
    /// Run one TTL-reaper tick across every shard. Required call cadence in
    /// `Manual` mode (~10×/s to match Redis `hz=10`). Returns the summed stats.
    pub fn tick(&self) -> ExpireStats {
        let mut total = ExpireStats::default();
        for (shard_i, shard) in self.shards.iter().enumerate() {
            #[cfg(not(all(feature = "index", feature = "persist", not(target_arch = "wasm32"))))]
            let _ = shard_i;
            let stats = {
                let mut g = lock_write(shard);
                // Tiering upkeep: budget re-resolution + the index/view
                // floor feed, then the tick continuation of the
                // budgeted spill.
                #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
                crate::shard::tier_tick_upkeep(&mut g, self.config.tier_budget, self.shards.len());
                let _ = g.store.demote_step();
                let _ = g.store.tier_compact_tick();
                // The window tick rides the manual cadence exactly as it
                // rides the background reaper's — a Manual-mode store
                // with a windowed table must slide too, not silently
                // stay all-hot.
                #[cfg(all(feature = "index", feature = "persist", not(target_arch = "wasm32")))]
                if let Some(dir) = &self.config.data_dir {
                    let inner = &mut *g;
                    crate::ops_index_window::window_tick(
                        &mut inner.idx_segs,
                        &mut inner.store,
                        &mut inner.aof,
                        &self.tables,
                        &kevy_persist::layout::segs_dir(dir, shard_i),
                    );
                }
                g.store.tick_expire(self.config.reaper_samples, self.config.reaper_max_rounds)
            };
            total.sampled += stats.sampled;
            total.expired += stats.expired;
            // Auto-rewrite rides the caller-driven tick in Manual mode; the
            // non-blocking path releases the lock for the disk spill.
            #[cfg(feature = "persist")]
            crate::reaper::concurrent_auto_rewrite(
                shard,
                kevy_persist::RewritePolicy {
                    pct: self.config.auto_aof_rewrite_pct,
                    min_size: self.config.auto_aof_rewrite_min_size,
                    bytes: self.config.auto_aof_rewrite_bytes,
                    interval_secs: self.config.auto_aof_rewrite_interval_secs,
                },
                self.config.metric_sink.as_ref(),
            );
        }
        total
    }
}

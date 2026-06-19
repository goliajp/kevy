//! Durability methods on [`Store`] — `BGREWRITEAOF` and `SAVE` — plus
//! their per-shard helpers. Extracted from `store.rs` to keep that file
//! under the 500-LOC project ceiling. Behaviour is unchanged from the
//! pre-split layout; this module hosts long-running disk paths
//! separately from the hot lock/dispatch surface in `store.rs`.

use std::io;
use std::sync::RwLock;
use std::time::Instant;

use kevy_persist::RewriteStats;

use crate::metric::KevyMetric;
use crate::store::{Inner, Store, lock_write};

impl Store {
    /// `BGREWRITEAOF`: rebuild every shard's AOF from current state.
    /// Synchronous. Returns the summed stats (`None` if persistence is off /
    /// no shard rewrote).
    pub fn rewrite_aof(&self) -> io::Result<Option<RewriteStats>> {
        let mut agg: Option<RewriteStats> = None;
        for shard in self.shards.iter() {
            let start = Instant::now();
            // Phase 1 (locked): freeze the COW view + start the tee —
            // O(n)-shallow, no serialization under the lock.
            let (view, tmp, before_bytes) = {
                let mut g = lock_write(shard);
                let Inner { store, aof, bus: _ } = &mut *g;
                let Some(aof) = aof else { continue };
                if aof.is_rewriting() {
                    continue;
                }
                let before = aof.size_bytes();
                let view = store.collect_snapshot();
                (view, aof.begin_view_rewrite()?, before)
            };
            // Phase 2 (unlocked): serialize + fsync the compacted log.
            let keys = match kevy_persist::dump_aof(&tmp, &view) {
                Ok((keys, _)) => keys,
                Err(e) => {
                    let mut g = lock_write(shard);
                    if let Some(aof) = &mut g.aof {
                        aof.abort_concurrent_rewrite();
                    }
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
            };
            // Phase 3 (locked): append the tee'd diff and swap.
            let mut g = lock_write(shard);
            let Some(aof) = &mut g.aof else { continue };
            let stats = match aof.finish_concurrent_rewrite(&tmp, keys) {
                Ok(s) => s,
                Err(e) => {
                    aof.abort_concurrent_rewrite();
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
            };
            if let Some(sink) = &self.config.metric_sink {
                sink.emit(KevyMetric::Rewrite {
                    keys: stats.keys,
                    before_bytes,
                    after_bytes: stats.bytes,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                });
            }
            let acc = agg.get_or_insert(RewriteStats { keys: 0, bytes: 0 });
            acc.keys += stats.keys;
            acc.bytes += stats.bytes;
        }
        Ok(agg)
    }

    /// Snapshot every shard to its `dump-{i}.rdb` (single shard: the configured
    /// name), atomically. `Ok(false)` when persistence is disabled.
    pub fn save_snapshot(&self) -> io::Result<bool> {
        let Some(dir) = self.config.data_dir.as_ref() else {
            return Ok(false);
        };
        let n = self.shards.len();
        for (i, shard) in self.shards.iter().enumerate() {
            let name = if n == 1 {
                self.config.snapshot_filename.clone()
            } else {
                kevy_persist::layout::snapshot_file(i)
            };
            save_shard_snapshot(shard, &dir.join(name))?;
        }
        Ok(true)
    }
}

/// Save one shard's snapshot with the snapshot+log contract intact:
/// after a successful save the AOF holds **only post-collect writes**,
/// so a restart replays them over the snapshot without double-applying
/// history (non-idempotent commands like RPUSH duplicated before this).
///
/// Phase 1 (write lock): freeze the COW view + start the AOF tee — no
/// write may land between the two (the tee atomicity contract). Phase 2
/// (unlocked): serialize the view to the snapshot's durable tmp.
/// Phase 3 (write lock): commit — snapshot rename and tee'd AOF reset
/// adjacent, so the snapshot/log commit window stays microseconds.
pub(crate) fn save_shard_snapshot(
    shard: &RwLock<Inner>,
    path: &std::path::Path,
) -> io::Result<()> {
    let (view, reset_tmp) = freeze_for_save(shard)?;
    let tmp = match kevy_persist::write_snapshot_tmp(&view, path) {
        Ok(t) => t,
        Err(e) => {
            if reset_tmp.is_some()
                && let Some(aof) = &mut lock_write(shard).aof
            {
                aof.abort_concurrent_rewrite();
            }
            return Err(e);
        }
    };
    let mut g = lock_write(shard);
    std::fs::rename(&tmp, path)?;
    if let (Some(reset), Some(aof)) = (reset_tmp, &mut g.aof) {
        let swap = kevy_persist::write_aof_base(&reset)
            .and_then(|()| aof.finish_concurrent_rewrite(&reset, 0));
        if let Err(e) = swap {
            aof.abort_concurrent_rewrite();
            let _ = std::fs::remove_file(&reset);
            return Err(e);
        }
    }
    Ok(())
}

/// Phase-1 helper: collect the view and start the tee under one write
/// lock. A racing background auto-rewrite owns the tee; it runs its
/// slow half off-lock and finishes in milliseconds, so wait it out
/// (bounded) rather than saving a snapshot whose log would double-
/// apply on replay.
fn freeze_for_save(
    shard: &RwLock<Inner>,
) -> io::Result<(kevy_store::SnapshotView, Option<std::path::PathBuf>)> {
    for _ in 0..2000 {
        {
            let mut g = lock_write(shard);
            let Inner { store, aof, .. } = &mut *g;
            match aof {
                Some(a) if a.is_rewriting() => {} // racing rewrite — retry
                Some(a) => {
                    let view = store.collect_snapshot();
                    return Ok((view, Some(a.begin_view_rewrite()?)));
                }
                None => return Ok((store.collect_snapshot(), None)),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "kevy-embedded: AOF rewrite still in flight after 10s; snapshot aborted",
    ))
}

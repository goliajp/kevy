//! Durability methods on [`Store`] — `BGREWRITEAOF` and `SAVE` — plus
//! their per-shard helpers. Extracted from `store.rs` to keep that file
//! under the 500-LOC project ceiling. Behaviour is unchanged from the
//! pre-split layout; this module hosts long-running disk paths
//! separately from the hot lock/dispatch surface in `store.rs`.

use crate::KevyResult;
use std::io;
use std::sync::RwLock;
use std::time::Instant;

use kevy_persist::{Argv, RewriteStats};

use crate::metric::KevyMetric;
use crate::store::{Inner, Store, lock_write};

impl Store {
    /// Durability barrier: flush + `fdatasync` every shard's
    /// AOF now, regardless of the configured `appendfsync` policy. On
    /// `Ok(())`, every write acknowledged before this call is on
    /// stable storage. The `EverySec` serving-store idiom:
    ///
    /// ```ignore
    /// store.atomic(|c| { /* critical write */ Ok(()) })?;
    /// store.fsync_aof()?; // durable-on-ack for THIS block only
    /// ```
    ///
    /// Cost: one `fdatasync` per dirty shard; a no-op on clean shards.
    /// Under `appendfsync = always` it is a no-op (already durable).
    pub fn fsync_aof(&self) -> KevyResult<()> {
        for shard in self.shards.iter() {
            let mut g = lock_write(shard);
            if let Some(aof) = &mut g.aof {
                aof.sync_now()?;
            }
        }
        Ok(())
    }

    /// `BGREWRITEAOF`: rebuild every shard's AOF from current state.
    /// Synchronous (despite the RESP name). Returns the summed stats
    /// (`None` if persistence is off / no shard rewrote). Shards mid
    /// rewrite are skipped. Emits [`KevyMetric::Rewrite`] per shard.
    pub fn rewrite_aof(&self) -> KevyResult<Option<RewriteStats>> {
        let mut agg: Option<RewriteStats> = None;
        for shard in self.shards.iter() {
            if let Some(stats) = self.rewrite_one_shard(shard)? {
                let acc = agg.get_or_insert(RewriteStats { keys: 0, bytes: 0 });
                acc.keys += stats.keys;
                acc.bytes += stats.bytes;
            }
        }
        Ok(agg)
    }

    /// One shard's synchronous three-phase rewrite. `Ok(None)` =
    /// skipped (persistence off / already mid-rewrite).
    fn rewrite_one_shard(&self, shard: &RwLock<Inner>) -> KevyResult<Option<RewriteStats>> {
        let start = Instant::now();
        // Phase 1 (locked): freeze the COW view + start the tee —
        // O(n)-shallow, no serialization under the lock.
        let (view, tmp, before_bytes) = {
            let mut g = lock_write(shard);
            let Inner { store, aof, .. } = &mut *g;
            let Some(aof) = aof else { return Ok(None) };
            if aof.is_rewriting() {
                return Ok(None);
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
                return Err(e.into());
            }
        };
        // Phase 3 (locked): append the tee'd diff and swap.
        let mut g = lock_write(shard);
        let Some(aof) = &mut g.aof else { return Ok(None) };
        let stats = match aof.finish_concurrent_rewrite(&tmp, keys) {
            Ok(s) => s,
            Err(e) => {
                aof.abort_concurrent_rewrite();
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
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
        Ok(Some(stats))
    }

    /// Apply one AOF-format command frame directly to the keyspace — the
    /// programmatic face of AOF replay, for hosts that store the log
    /// themselves (targets without a filesystem, where the embedding
    /// host reads the log back and feeds it in frame by frame on open).
    ///
    /// Speaks the same verb set `Store::open` replays from an on-disk
    /// AOF; unknown verbs are skipped (forward compatibility with logs
    /// written by a newer kevy). Keyed verbs route to the owning shard;
    /// `FLUSHALL`/`FLUSHDB` reach every shard. The frame is **not**
    /// re-appended to any AOF — this is the read-back half of the pump,
    /// so re-logging would double-apply on the next replay.
    pub fn apply_frame(&self, args: &Argv) {
        let Some(verb) = args.first() else { return };
        if verb.eq_ignore_ascii_case(b"FLUSHALL") || verb.eq_ignore_ascii_case(b"FLUSHDB") {
            for shard in self.shards.iter() {
                crate::replay::apply(&mut lock_write(shard).store, args);
            }
            return;
        }
        if let Some(key) = args.get(1) {
            crate::replay::apply(&mut self.wshard(key).store, args);
        }
    }

    /// Serialize the whole keyspace into an in-memory compacted AOF image
    /// (magic header + one command stream per key — the same bytes an
    /// AOF rewrite puts on disk). The write half of host-mediated
    /// persistence: hosts without a filesystem hand this buffer to their
    /// own storage, replacing the accumulated append log, and feed it
    /// back through [`Self::apply_frame`] on the next open.
    ///
    /// Each shard is frozen copy-on-write and serialized off-lock, so
    /// concurrent readers and writers on other shards are not blocked
    /// for the duration of the dump.
    pub fn dump_aof_buf(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, shard) in self.shards.iter().enumerate() {
            let view = lock_write(shard).store.collect_snapshot();
            // V1 on purpose: this image feeds the wasm door's host-mediated
            // log, whose incremental browser-side parser speaks bare RESP.
            // Upgrading that pump to v2 envelopes is the arc's T5b item.
            let (buf, _keys) =
                kevy_persist::dump_store_to_buf(&view, kevy_persist::AofFormat::V1);
            if i == 0 {
                out = buf;
            } else {
                // One magic header per image, not per shard.
                out.extend_from_slice(&buf[kevy_persist::AOF_MAGIC.len()..]);
            }
        }
        out
    }

    /// Snapshot every shard to its `dump-{i}.rdb`, atomically. `Ok(false)`
    /// when persistence is disabled.
    pub fn save_snapshot(&self) -> KevyResult<bool> {
        let Some(dir) = self.config.data_dir.as_ref() else {
            return Ok(false);
        };
        for (i, shard) in self.shards.iter().enumerate() {
            save_shard_snapshot(shard, &kevy_persist::layout::snapshot_path(dir, i))?;
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
) -> KevyResult<()> {
    let (view, reset_tmp) = freeze_for_save(shard)?;
    let tmp = match kevy_persist::write_snapshot_tmp(&view, path) {
        Ok(t) => t,
        Err(e) => {
            if reset_tmp.is_some()
                && let Some(aof) = &mut lock_write(shard).aof
            {
                aof.abort_concurrent_rewrite();
            }
            return Err(e.into());
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
            return Err(e.into());
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
) -> KevyResult<(kevy_store::SnapshotView, Option<std::path::PathBuf>)> {
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
    )
    .into())
}

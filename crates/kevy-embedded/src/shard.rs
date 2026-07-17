//! Keyspace sharding for the embedded store: routing + per-shard persistence
//! bring-up (load / migrate). Each shard is a fully independent
//! `kevy_store::Store` + AOF behind its own lock (shared-nothing), so
//! concurrent access on different shards never contends. `n == 1` is the
//! original single-shard layout (one snapshot + one AOF under the configured
//! filenames, `dump-0.rdb` / `aof-0.aof` by default). `n > 1` keeps per-shard
//! `aof-{i}.aof` + a `shards.meta` recording the count; the first open at
//! `n > 1` re-shards a legacy single AOF into per-shard files.
//!
//! Dir interop with the `kevy` server: a single-shard dir is byte-identical
//! to the server's 1-thread layout, and `n == 1` records `shards.meta` too
//! so neither side needs inference.

use std::io;
#[cfg(feature = "persist")]
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
#[cfg(feature = "persist")]
use std::time::Instant;

use kevy_hash::KevyHash;
#[cfg(feature = "persist")]
use kevy_persist::reshard::{ShardLayout, commit_reshard, merge_sources, recover_journal};
#[cfg(feature = "persist")]
use kevy_persist::{
    Aof, Routing, ShardsMeta, layout, layout::infer_files_n, load_snapshot, read_shards_meta,
    replay_aof, write_shards_meta,
};
use kevy_store::Store as Keyspace;

use crate::config::{Config, TtlReaperMode};
#[cfg(feature = "persist")]
use crate::metric::{KevyMetric, OpenReport};
use crate::store::Inner;

/// Route a key to its shard. `n == 1` short-circuits to 0; power-of-two `n`
/// uses a mask, else modulo. Same `KevyHash` the server's shard router uses.
#[inline]
pub(crate) fn shard_idx(key: &[u8], n: usize) -> usize {
    if n == 1 {
        return 0;
    }
    let h = key.kevy_hash() as usize;
    if n.is_power_of_two() {
        h & (n - 1)
    } else {
        h % n
    }
}

/// The embedded store's file layout for [`kevy_persist::reshard`]: the
/// standard per-shard names (`dump-{i}.rdb` / `aof-{i}.aof`). `n == 1`
/// coincides with shard 0's names, which are also the legacy single-file
/// names — old single-file dirs load without migration.
#[cfg(feature = "persist")]
struct EmbLayout;

#[cfg(feature = "persist")]
impl ShardLayout for EmbLayout {
    fn snapshot_path(&self, dir: &Path, i: usize, _n: usize) -> PathBuf {
        layout::snapshot_path(dir, i)
    }
    fn aof_path(&self, dir: &Path, i: usize, _n: usize) -> PathBuf {
        layout::aof_path(dir, i)
    }
}

fn fresh_keyspace(config: &Config) -> Keyspace {
    let mut s = Keyspace::new();
    s.set_max_memory(config.maxmemory, config.eviction_policy);
    s.set_cached_clock(matches!(config.ttl_reaper, TtlReaperMode::Background));
    s
}

/// Build the `n` shard `Inner`s for `config`, loading / migrating persistence.
/// The `bus` lives on shard 0 (pub/sub is process-wide, not sharded); other
/// shards get an idle bus that is never touched.
pub(crate) fn build_shards(config: &Config) -> io::Result<(Vec<Arc<RwLock<Inner>>>, OpenReport)> {
    let n = config.shards.max(1);
    #[allow(unused_mut)] // mut is the persist path's (load/reshard) need
    let mut stores: Vec<Keyspace> = (0..n).map(|_| fresh_keyspace(config)).collect();

    // Without the `persist` feature the build is always pure in-memory.
    #[cfg(not(feature = "persist"))]
    return Ok((into_inners_mem(stores), OpenReport::default()));

    #[cfg(feature = "persist")]
    build_shards_persist(config, n, stores)
}

/// Persistence bring-up half of [`build_shards`]: load / migrate the
/// on-disk layout, then open each shard's live AOF.
#[cfg(feature = "persist")]
fn build_shards_persist(
    config: &Config,
    n: usize,
    mut stores: Vec<Keyspace>,
) -> io::Result<(Vec<Arc<RwLock<Inner>>>, OpenReport)> {
    let Some(dir) = config.data_dir.clone() else {
        // Pure in-memory: no persistence, no AOF.
        return Ok((into_inners(stores, (0..n).map(|_| None).collect()), OpenReport::default()));
    };
    std::fs::create_dir_all(&dir)?;
    // Complete (or safely discard) a reshard a crash interrupted, before
    // reading the layout — same roll-forward the server runtime does.
    recover_journal(&dir, &EmbLayout)?;
    let mut report = load_or_reshard(&dir, config, n, &mut stores)?;

    // Open each shard's live AOF for append (if persistence is on). The
    // open repairs (quarantines + truncates) any dropped tail replay just
    // tolerated — collect the quarantine paths into the report.
    let aofs: Vec<Option<Aof>> = if config.aof {
        (0..n)
            .map(|i| {
                Aof::open_with_repair(
                    &layout::aof_path(&dir, i),
                    config.appendfsync,
                    config.replay_resync,
                )
                .map(Some)
            })
            .collect::<io::Result<_>>()?
    } else {
        (0..n).map(|_| None).collect()
    };
    for aof in aofs.iter().flatten() {
        if let Some(q) = aof.open_quarantine() {
            report.quarantine_paths.push(q.to_path_buf());
        }
    }
    Ok((into_inners(stores, aofs), report))
}

/// Read the shard layout meta and either load in place (same layout)
/// or re-shard (losslessly) into the configured `n`.
#[cfg(feature = "persist")]
fn load_or_reshard(
    dir: &Path,
    config: &Config,
    n: usize,
    stores: &mut [Keyspace],
) -> io::Result<OpenReport> {
    let meta_path = layout::shards_meta_path(dir);
    let prev = read_shards_meta(&meta_path);
    // The embedded store always routes by KevyHash; a dir written by a
    // slots-routing server re-shards (losslessly) on first embedded open.
    let same_layout = match prev {
        Some(m) => m.n == n && m.routing == Routing::KevyHash,
        // No meta + n==1: the single-file layout — unless the files say
        // multi-shard (a meta-less pre-1.5 server dir). Loading only
        // shard 0 of those silently dropped (k-1)/k of the keyspace.
        None => n == 1 && infer_files_n(dir) <= 1,
    };

    if same_layout {
        let report = load_in_place(dir, config, n, stores)?;
        write_shards_meta(&meta_path, ShardsMeta { n, routing: Routing::KevyHash })?;
        return Ok(report);
    }
    {
        let src_n = prev.map(|m| m.n).or_else(|| {
            let k = infer_files_n(dir);
            (k > 1).then_some(k)
        });
        // The commit also records the new layout — including n == 1: a
        // stale meta from a larger prior n would otherwise trigger a second
        // re-shard next open, whose sources were already renamed to
        // `.premigration` (the shrink-to-one open would come up empty).
        reshard(dir, config, n, src_n, stores)
    }
}

/// Same-layout load: each shard reads its own snapshot + AOF directly.
#[cfg(feature = "persist")]
fn load_in_place(
    dir: &Path,
    config: &Config,
    _n: usize,
    stores: &mut [Keyspace],
) -> io::Result<OpenReport> {
    let mut report = OpenReport::default();
    let start = Instant::now();
    for (i, store) in stores.iter_mut().enumerate() {
        let snap = layout::snapshot_path(dir, i);
        if snap.exists() {
            load_snapshot(store, &snap)?;
        }
        let aof = layout::aof_path(dir, i);
        if aof.exists() {
            let apply = |args: kevy_persist::Argv| crate::replay::apply(store, &args);
            let r = if config.replay_resync {
                kevy_persist::replay_aof_resync(&aof, apply)?
            } else {
                replay_aof(&aof, apply)?
            };
            report.replayed_commands += r.commands;
            report.replayed_bytes += r.replayed_bytes;
            report.dropped_bytes += r.dropped_bytes;
            report.corrupt |= r.corrupt;
            report.resynced_bytes += r.resynced_ranges.iter().map(|(a, b)| b - a).sum::<u64>();
        }
    }
    report.elapsed_ms = start.elapsed().as_millis() as u64;
    emit_replay(config, &report);
    Ok(report)
}

/// Re-shard: load every source file into one temp keyspace, redistribute
/// each key to its target shard, then hand the crash-safe commit to
/// [`kevy_persist::reshard`] — per-shard snapshots land under `.reshard`
/// temp names, a durable journal marks the commit point, and only then are
/// the sources backed up (`.premigration.<nanos>`) and the temps finalized.
/// A crash at any point either leaves the old layout intact or is rolled
/// forward by `build_shards`' recovery on the next open. Each shard's fresh
/// AOF opens after this returns; the snapshot is the full migrated state.
#[cfg(feature = "persist")]
fn reshard(
    dir: &Path,
    config: &Config,
    n: usize,
    prev_n: Option<usize>,
    stores: &mut [Keyspace],
) -> io::Result<OpenReport> {
    let lay = EmbLayout;
    let mut temp = fresh_keyspace(config);
    let mut total_cmds = 0u64;
    let start = Instant::now();
    // Source layout: prior shard files, or a legacy single AOF/snapshot.
    let src_n = prev_n.unwrap_or(1);
    merge_sources(dir, src_n, &lay, &mut temp, |store, args| {
        total_cmds += 1;
        crate::replay::apply(store, &args);
    })?;
    // Replay-metric byte count: the source AOFs (sizes read before the
    // commit renames them away). Per-file drop/corrupt detail is not
    // plumbed through the merge path — a migrating open reports totals.
    let total_bytes = (0..src_n)
        .map(|i| lay.aof_path(dir, i, src_n))
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let mut report = OpenReport::default();
    report.replayed_commands = total_cmds;
    report.replayed_bytes = total_bytes;
    report.elapsed_ms = start.elapsed().as_millis() as u64;
    emit_replay(config, &report);

    // Redistribute the merged keyspace into the target shards.
    temp.snapshot_each(|key, value, ttl_ms| {
        stores[shard_idx(key, n)].load_value(key, value, ttl_ms);
    });

    commit_reshard(dir, src_n, ShardsMeta { n, routing: Routing::KevyHash }, stores, &lay)?;
    Ok(report)
}

#[cfg(feature = "persist")]
fn emit_replay(config: &Config, report: &OpenReport) {
    if let Some(sink) = &config.metric_sink {
        sink.emit(KevyMetric::Replay {
            commands: report.replayed_commands,
            bytes: report.replayed_bytes + report.dropped_bytes,
            elapsed_ms: report.elapsed_ms,
            dropped_bytes: report.dropped_bytes,
            corrupt: report.corrupt,
        });
    }
}

#[cfg(feature = "persist")]
fn into_inners(stores: Vec<Keyspace>, aofs: Vec<Option<Aof>>) -> Vec<Arc<RwLock<Inner>>> {
    stores
        .into_iter()
        .zip(aofs)
        .map(|(store, aof)| Arc::new(RwLock::new(Inner::new(store, aof))))
        .collect()
}

/// In-memory-only `Inner` build (the whole story without `persist`).
#[cfg(not(feature = "persist"))]
fn into_inners_mem(stores: Vec<Keyspace>) -> Vec<Arc<RwLock<Inner>>> {
    stores
        .into_iter()
        .map(|store| Arc::new(RwLock::new(Inner::new(store))))
        .collect()
}

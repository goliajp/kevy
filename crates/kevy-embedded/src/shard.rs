//! Keyspace sharding for the embedded store: routing + per-shard persistence
//! bring-up (load / migrate). Each shard is a fully independent
//! `kevy_store::Store` + AOF behind its own lock (shared-nothing), so
//! concurrent access on different shards never contends. `n == 1` is the
//! original single-shard layout (one snapshot + one AOF under the configured
//! filenames, `dump-0.rdb` / `aof-0.aof` by default). `n > 1` keeps per-shard
//! `aof-{i}.aof` + a `shards.meta` recording the count; the first open at
//! `n > 1` re-shards a legacy single AOF into per-shard files.
//!
//! Dir interop with the `kevy` server: under the default filenames a
//! single-shard dir is byte-identical to the server's 1-thread layout, and
//! `n == 1` records `shards.meta` too so neither side needs inference.
//! Custom `with_aof_filename` / `with_snapshot_filename` names opt out of
//! that interop (the dir stays meta-less — a server can't find the files).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use kevy_hash::KevyHash;
use kevy_persist::reshard::{ShardLayout, commit_reshard, merge_sources, recover_journal};
use kevy_persist::{
    Aof, Routing, ShardsMeta, layout, layout::infer_files_n, load_snapshot, read_shards_meta,
    replay_aof, write_shards_meta,
};
use kevy_store::Store as Keyspace;

use crate::config::{Config, TtlReaperMode};
use crate::metric::KevyMetric;
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

fn aof_path(dir: &Path, config: &Config, i: usize, n: usize) -> PathBuf {
    if n == 1 {
        dir.join(&config.aof_filename) // back-compat: the original single file
    } else {
        layout::aof_path(dir, i)
    }
}

fn snapshot_path(dir: &Path, config: &Config, i: usize, n: usize) -> PathBuf {
    if n == 1 {
        dir.join(&config.snapshot_filename)
    } else {
        layout::snapshot_path(dir, i)
    }
}

/// The embedded store's file layout for [`kevy_persist::reshard`]: the
/// standard per-shard names, except `n == 1` keeps the configured
/// single-file names (the `with_*_filename` back-compat knobs).
struct EmbLayout<'a>(&'a Config);

impl ShardLayout for EmbLayout<'_> {
    fn snapshot_path(&self, dir: &Path, i: usize, n: usize) -> PathBuf {
        snapshot_path(dir, self.0, i, n)
    }
    fn aof_path(&self, dir: &Path, i: usize, n: usize) -> PathBuf {
        aof_path(dir, self.0, i, n)
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
pub(crate) fn build_shards(config: &Config) -> io::Result<Vec<Arc<RwLock<Inner>>>> {
    let n = config.shards.max(1);
    let mut stores: Vec<Keyspace> = (0..n).map(|_| fresh_keyspace(config)).collect();

    let Some(dir) = config.data_dir.clone() else {
        // Pure in-memory: no persistence, no AOF.
        return Ok(into_inners(stores, (0..n).map(|_| None).collect()));
    };
    std::fs::create_dir_all(&dir)?;
    // Complete (or safely discard) a reshard a crash interrupted, before
    // reading the layout — same roll-forward the server runtime does.
    recover_journal(&dir, &EmbLayout(config))?;

    let meta_path = layout::shards_meta_path(&dir);
    let prev = read_shards_meta(&meta_path);
    // Under the default filenames the n==1 layout coincides with shard 0's,
    // so the dir is server-readable; custom names opt out of that interop
    // and stay meta-less (a meta would point the server at files that
    // don't exist).
    let sharded_names = config.snapshot_filename == layout::snapshot_file(0)
        && config.aof_filename == layout::aof_file(0);
    // The embedded store always routes by KevyHash; a dir written by a
    // slots-routing server re-shards (losslessly) on first embedded open.
    let same_layout = match prev {
        Some(m) => m.n == n && m.routing == Routing::KevyHash,
        // No meta + n==1: the single-file layout — unless the files say
        // multi-shard (a meta-less pre-1.5 server dir). Loading only
        // shard 0 of those silently dropped (k-1)/k of the keyspace.
        None => n == 1 && infer_files_n(&dir) <= 1,
    };

    if same_layout {
        load_in_place(&dir, config, n, &mut stores)?;
        if n > 1 || sharded_names {
            write_shards_meta(&meta_path, ShardsMeta { n, routing: Routing::KevyHash })?;
        }
    } else {
        let src_n = prev.map(|m| m.n).or_else(|| {
            let k = infer_files_n(&dir);
            (k > 1).then_some(k)
        });
        // The commit also records the new layout — including n == 1: a
        // stale meta from a larger prior n would otherwise trigger a second
        // re-shard next open, whose sources were already renamed to
        // `.premigration` (the shrink-to-one open would come up empty).
        reshard(&dir, config, n, src_n, &mut stores)?;
    }

    // Open each shard's live AOF for append (if persistence is on).
    let aofs: Vec<Option<Aof>> = if config.aof {
        (0..n)
            .map(|i| Aof::open(&aof_path(&dir, config, i, n), config.appendfsync).map(Some))
            .collect::<io::Result<_>>()?
    } else {
        (0..n).map(|_| None).collect()
    };
    Ok(into_inners(stores, aofs))
}

/// Same-layout load: each shard reads its own snapshot + AOF directly.
fn load_in_place(dir: &Path, config: &Config, n: usize, stores: &mut [Keyspace]) -> io::Result<()> {
    let mut total_cmds = 0u64;
    let mut total_bytes = 0u64;
    let start = Instant::now();
    for (i, store) in stores.iter_mut().enumerate() {
        let snap = snapshot_path(dir, config, i, n);
        if snap.exists() {
            load_snapshot(store, &snap)?;
        }
        let aof = aof_path(dir, config, i, n);
        if aof.exists() {
            total_bytes += std::fs::metadata(&aof).map_or(0, |m| m.len());
            replay_aof(&aof, |args| {
                total_cmds += 1;
                crate::replay::apply(store, &args);
            })?;
        }
    }
    emit_replay(config, total_cmds, total_bytes, start);
    Ok(())
}

/// Re-shard: load every source file into one temp keyspace, redistribute
/// each key to its target shard, then hand the crash-safe commit to
/// [`kevy_persist::reshard`] — per-shard snapshots land under `.reshard`
/// temp names, a durable journal marks the commit point, and only then are
/// the sources backed up (`.premigration.<nanos>`) and the temps finalized.
/// A crash at any point either leaves the old layout intact or is rolled
/// forward by `build_shards`' recovery on the next open. Each shard's fresh
/// AOF opens after this returns; the snapshot is the full migrated state.
fn reshard(
    dir: &Path,
    config: &Config,
    n: usize,
    prev_n: Option<usize>,
    stores: &mut [Keyspace],
) -> io::Result<()> {
    let lay = EmbLayout(config);
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
    // commit renames them away).
    let total_bytes = (0..src_n)
        .map(|i| lay.aof_path(dir, i, src_n))
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    emit_replay(config, total_cmds, total_bytes, start);

    // Redistribute the merged keyspace into the target shards.
    temp.snapshot_each(|key, value, ttl_ms| {
        stores[shard_idx(key, n)].load_value(key, value, ttl_ms);
    });

    commit_reshard(dir, src_n, ShardsMeta { n, routing: Routing::KevyHash }, stores, &lay)?;
    Ok(())
}

fn emit_replay(config: &Config, commands: u64, bytes: u64, start: Instant) {
    if let Some(sink) = &config.metric_sink {
        sink.emit(KevyMetric::Replay {
            commands,
            bytes,
            elapsed_ms: start.elapsed().as_millis() as u64,
        });
    }
}

fn into_inners(stores: Vec<Keyspace>, aofs: Vec<Option<Aof>>) -> Vec<Arc<RwLock<Inner>>> {
    stores
        .into_iter()
        .zip(aofs)
        .map(|(store, aof)| Arc::new(RwLock::new(Inner::new(store, aof))))
        .collect()
}

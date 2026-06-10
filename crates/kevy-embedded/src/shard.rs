//! Keyspace sharding for the embedded store: routing + per-shard persistence
//! bring-up (load / migrate). Each shard is a fully independent
//! `kevy_store::Store` + AOF behind its own lock (shared-nothing), so
//! concurrent access on different shards never contends. `n == 1` is the
//! original single-shard layout (single `aof-0.aof`, no `shards.meta`) — zero
//! change, zero migration. `n > 1` keeps per-shard `aof-{i}.aof` + a
//! `shards.meta` recording the count; the first open at `n > 1` re-shards a
//! legacy single AOF into per-shard files.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use kevy_hash::KevyHash;
use kevy_persist::{
    Aof, Routing, ShardsMeta, load_snapshot, read_shards_meta, replay_aof, write_shards_meta,
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
        dir.join(format!("aof-{i}.aof"))
    }
}

fn snapshot_path(dir: &Path, config: &Config, i: usize, n: usize) -> PathBuf {
    if n == 1 {
        dir.join(&config.snapshot_filename)
    } else {
        dir.join(format!("dump-{i}.rdb"))
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

    let meta_path = dir.join("shards.meta");
    let prev = read_shards_meta(&meta_path);
    // The embedded store always routes by KevyHash; a dir written by a
    // slots-routing server re-shards (losslessly) on first embedded open.
    let same_layout = match prev {
        Some(m) => m.n == n && m.routing == Routing::KevyHash,
        None => n == 1, // no meta + n==1 = the untouched single-file layout
    };

    if same_layout {
        load_in_place(&dir, config, n, &mut stores)?;
    } else {
        reshard(&dir, config, n, prev.map(|m| m.n), &mut stores)?;
        // Always record the new layout — including n == 1: a stale meta
        // from a larger prior n would otherwise trigger a second re-shard
        // next open, whose sources were already renamed to `.premigration`
        // (the shrink-to-one open would come up empty).
        write_shards_meta(&meta_path, ShardsMeta { n, routing: Routing::KevyHash })?;
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
            total_bytes += std::fs::metadata(&aof).map(|m| m.len()).unwrap_or(0);
            replay_aof(&aof, |args| {
                total_cmds += 1;
                crate::replay::apply(store, &args);
            })?;
        }
    }
    emit_replay(config, total_cmds, total_bytes, start);
    Ok(())
}

/// Re-shard: load every source file into one temp keyspace, redistribute each
/// key to its target shard, then rewrite each shard's AOF from its slice. The
/// source files are backed up (`.premigration.<nanos>`) before being replaced.
fn reshard(
    dir: &Path,
    config: &Config,
    n: usize,
    prev_n: Option<usize>,
    stores: &mut [Keyspace],
) -> io::Result<()> {
    let mut temp = fresh_keyspace(config);
    let mut total_cmds = 0u64;
    let mut total_bytes = 0u64;
    let start = Instant::now();
    // Source layout: prior shard files, or a legacy single AOF/snapshot.
    let src_n = prev_n.unwrap_or(1);
    let mut sources: Vec<PathBuf> = Vec::new();
    for i in 0..src_n {
        let snap = snapshot_path(dir, config, i, src_n);
        if snap.exists() {
            load_snapshot(&mut temp, &snap)?;
            sources.push(snap);
        }
        let aof = aof_path(dir, config, i, src_n);
        if aof.exists() {
            total_bytes += std::fs::metadata(&aof).map(|m| m.len()).unwrap_or(0);
            replay_aof(&aof, |args| {
                total_cmds += 1;
                crate::replay::apply(&mut temp, &args);
            })?;
            sources.push(aof);
        }
    }
    emit_replay(config, total_cmds, total_bytes, start);

    // Redistribute the merged keyspace into the target shards.
    temp.snapshot_each(|key, value, ttl_ms| {
        stores[shard_idx(key, n)].load_value(key, value, ttl_ms);
    });

    // Back up sources, then materialize each shard's compacted AOF.
    let stamp = backup_stamp();
    for src in &sources {
        let mut bak = src.clone().into_os_string();
        bak.push(format!(".premigration.{stamp}"));
        let _ = std::fs::rename(src, &bak);
    }
    if config.aof {
        for (i, store) in stores.iter().enumerate() {
            let mut aof = Aof::open(&aof_path(dir, config, i, n), config.appendfsync)?;
            aof.rewrite_from(store)?;
        }
    }
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

fn backup_stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn into_inners(stores: Vec<Keyspace>, aofs: Vec<Option<Aof>>) -> Vec<Arc<RwLock<Inner>>> {
    stores
        .into_iter()
        .zip(aofs)
        .map(|(store, aof)| Arc::new(RwLock::new(Inner::new(store, aof))))
        .collect()
}

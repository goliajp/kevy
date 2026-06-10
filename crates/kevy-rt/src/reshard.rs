//! Server-side shard-layout bring-up: detect a `shards.meta` mismatch and
//! re-home every key before any shard thread spawns.
//!
//! Per-shard `dump-{i}.rdb` / `aof-{i}.aof` files are only readable under
//! the (shard count, routing scheme) that wrote them. Until now the server
//! recorded neither — restarting with a different `--threads` silently
//! stranded keys in files no shard would route to (the embedded store fixed
//! this in its B2 sharding; the server never did). Cluster mode adds a
//! second axis (KevyHash vs slots routing), so both are now recorded and a
//! mismatch triggers one centralized, lossless re-shard, modeled on the
//! embedded path: merge every source file into a temp store, redistribute
//! under the new layout, back sources up as `.premigration.<nanos>`, write
//! fresh per-shard snapshots, record the layout.

use crate::Commands;
use crate::reduce::shard_of;
use kevy_persist::{
    Routing, ShardsMeta, load_snapshot, read_shards_meta, replay_aof, save_snapshot,
    write_shards_meta,
};
use kevy_store::Store;
use std::io;
use std::path::{Path, PathBuf};

/// Ensure `dir`'s persisted layout matches `(n, routing)`, re-sharding once
/// if it doesn't. Called by `Runtime::run` before any shard thread spawns;
/// afterwards each shard loads its own files exactly as before. A reshard
/// interrupted by a crash is completed (or safely discarded) first — see
/// [`recover_journal`].
pub(crate) fn ensure_layout<C: Commands>(
    dir: &Path,
    n: usize,
    routing: Routing,
    commands: &C,
) -> io::Result<()> {
    let meta_path = dir.join("shards.meta");
    recover_journal(dir)?;
    let target = ShardsMeta { n, routing };
    let prev = match read_shards_meta(&meta_path) {
        Some(m) => m,
        // Legacy dir (server never wrote meta): the shard count is however
        // many per-shard files exist, the routing is the only scheme that
        // existed. An empty dir trivially "matches" — just record target.
        None => ShardsMeta {
            n: infer_legacy_n(dir),
            routing: Routing::KevyHash,
        },
    };
    if prev.n == 0 || prev == target {
        std::fs::create_dir_all(dir)?;
        return write_shards_meta(&meta_path, target);
    }
    reshard(dir, prev, target, commands)?;
    write_shards_meta(&meta_path, target)
}

/// Whether `dir` holds any kevy persistence artifacts (per-shard snapshot,
/// AOF, or a `shards.meta`). Gates layout reconciliation for pure in-memory
/// runs so they keep writing nothing.
pub(crate) fn has_kevy_files(dir: &Path) -> bool {
    infer_legacy_n(dir) > 0 || dir.join("shards.meta").exists()
}

/// Highest `dump-{i}.rdb` / `aof-{i}.aof` index + 1, or 0 for no files.
fn infer_legacy_n(dir: &Path) -> usize {
    let mut n = 0usize;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let idx = name
            .strip_prefix("dump-")
            .and_then(|r| r.strip_suffix(".rdb"))
            .or_else(|| name.strip_prefix("aof-").and_then(|r| r.strip_suffix(".aof")));
        if let Some(i) = idx.and_then(|s| s.parse::<usize>().ok()) {
            n = n.max(i + 1);
        }
    }
    n
}

/// Merge every `prev` source file into one temp store, redistribute under
/// `target`, write per-shard snapshots, then commit: back the sources up
/// and move the snapshots into place. AOFs are not rewritten — the snapshot
/// is the full state and each shard opens a fresh (empty) log on bring-up;
/// the old logs live on in the backups.
///
/// Crash-safe ordering (the old rename-sources-first order lost the whole
/// keyspace to a crash between the renames and the snapshot writes): new
/// snapshots land under temp `.reshard` names first, then a durable journal
/// marks the commit point, and only then are sources renamed away and the
/// temps finalized. A crash before the journal leaves the old layout fully
/// intact; a crash after it is completed by [`recover_journal`] on the next
/// start.
fn reshard<C: Commands>(
    dir: &Path,
    prev: ShardsMeta,
    target: ShardsMeta,
    commands: &C,
) -> io::Result<()> {
    // Stale `.reshard` temps from a pre-journal crash are dead weight —
    // the journal was never written, so that attempt never committed.
    for i in 0..target.n {
        let _ = std::fs::remove_file(dir.join(format!("dump-{i}.rdb.reshard")));
    }
    let mut temp = Store::new();
    let mut sources: Vec<PathBuf> = Vec::new();
    for i in 0..prev.n {
        let snap = dir.join(format!("dump-{i}.rdb"));
        if snap.exists() {
            load_snapshot(&mut temp, &snap)?;
            sources.push(snap);
        }
        let aof = dir.join(format!("aof-{i}.aof"));
        if aof.exists() {
            replay_aof(&aof, |args| {
                commands.dispatch(&mut temp, &args);
            })?;
            sources.push(aof);
        }
    }

    let mut stores: Vec<Store> = (0..target.n).map(|_| Store::new()).collect();
    let slots = target.routing == Routing::Slots;
    temp.snapshot_each(|key, value, ttl_ms| {
        stores[shard_of(key, target.n, slots)].load_value(key, value, ttl_ms);
    });

    for (i, store) in stores.iter().enumerate() {
        save_snapshot(store, &dir.join(format!("dump-{i}.rdb.reshard")))?;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    write_journal(dir, prev.n, target, stamp)?; // ── commit point ──
    finish_reshard(dir, prev.n, target, stamp)?;
    eprintln!(
        "kevy: re-sharded {} -> {} shards ({:?} -> {:?} routing); {} source file(s) backed up as .premigration.{stamp}",
        prev.n, target.n, prev.routing, target.routing, sources.len(),
    );
    Ok(())
}

const JOURNAL: &str = "reshard.journal";

/// Persist the reshard commit record durably (write + fsync) — once this
/// exists, the migration is committed and any crash is rolled *forward*.
fn write_journal(dir: &Path, prev_n: usize, target: ShardsMeta, stamp: u128) -> io::Result<()> {
    use std::io::Write;
    let routing = match target.routing {
        Routing::KevyHash => "kevyhash",
        Routing::Slots => "slots",
    };
    let body = format!(
        "kevy-reshard-journal v1\nstamp={stamp}\nprev_n={prev_n}\nn={}\nrouting={routing}\n",
        target.n,
    );
    let mut f = std::fs::File::create(dir.join(JOURNAL))?;
    f.write_all(body.as_bytes())?;
    f.sync_all()
}

/// The post-journal half of a reshard: rename every old-layout source to
/// its `.premigration.<stamp>` backup, move the `.reshard` snapshots into
/// place, record the new layout, drop the journal. Every step is an
/// idempotent rename/remove, so this can resume after a crash at any point.
fn finish_reshard(dir: &Path, prev_n: usize, target: ShardsMeta, stamp: u128) -> io::Result<()> {
    for i in 0..prev_n {
        let snap = dir.join(format!("dump-{i}.rdb"));
        // A plain dump file is an old source unless the new layout already
        // finalized this index (its `.reshard` temp is gone and i < n).
        let is_source =
            i >= target.n || dir.join(format!("dump-{i}.rdb.reshard")).exists();
        if is_source && snap.exists() {
            rename_to_backup(&snap, stamp)?;
        }
        let aof = dir.join(format!("aof-{i}.aof"));
        if aof.exists() {
            // Resharded layouts never carry AOFs (fresh logs open on
            // bring-up), so any AOF here is an old source.
            rename_to_backup(&aof, stamp)?;
        }
    }
    for i in 0..target.n {
        let tmp = dir.join(format!("dump-{i}.rdb.reshard"));
        if tmp.exists() {
            std::fs::rename(&tmp, dir.join(format!("dump-{i}.rdb")))?;
        }
    }
    write_shards_meta(&dir.join("shards.meta"), target)?;
    std::fs::remove_file(dir.join(JOURNAL))
}

fn rename_to_backup(src: &Path, stamp: u128) -> io::Result<()> {
    let mut bak = src.as_os_str().to_owned();
    bak.push(format!(".premigration.{stamp}"));
    std::fs::rename(src, &bak)
}

/// Startup half of the crash story: a journal on disk means a reshard
/// committed but didn't finish — roll it forward via [`finish_reshard`].
/// An unparsable journal means the crash hit mid-journal-write, i.e. the
/// commit point was never reached: the old layout is fully intact, so the
/// torn journal (and any `.reshard` temps, cleaned by the next `reshard`)
/// is safely discarded.
fn recover_journal(dir: &Path) -> io::Result<()> {
    let path = dir.join(JOURNAL);
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        // An unreadable-but-present journal must stop startup: pressing on
        // would re-run a reshard over partially-renamed sources.
        Err(e) => return Err(e),
    };
    match parse_journal(&body) {
        Some((prev_n, target, stamp)) => {
            finish_reshard(dir, prev_n, target, stamp)?;
            eprintln!(
                "kevy: completed interrupted re-shard to {} shards ({:?} routing)",
                target.n, target.routing,
            );
            Ok(())
        }
        None => std::fs::remove_file(&path),
    }
}

fn parse_journal(body: &str) -> Option<(usize, ShardsMeta, u128)> {
    let mut lines = body.lines();
    if lines.next() != Some("kevy-reshard-journal v1") {
        return None;
    }
    let mut stamp = None;
    let mut prev_n = None;
    let mut n = None;
    let mut routing = None;
    for line in lines {
        let (k, v) = line.split_once('=')?;
        match k {
            "stamp" => stamp = v.parse::<u128>().ok(),
            "prev_n" => prev_n = v.parse::<usize>().ok(),
            "n" => n = v.parse::<usize>().ok(),
            "routing" => {
                routing = match v {
                    "kevyhash" => Some(Routing::KevyHash),
                    "slots" => Some(Routing::Slots),
                    _ => None,
                }
            }
            _ => return None,
        }
    }
    Some((prev_n?, ShardsMeta { n: n?, routing: routing? }, stamp?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kevy-reshard-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").unwrap();
    }

    const TARGET: ShardsMeta = ShardsMeta { n: 2, routing: Routing::Slots };

    #[test]
    fn journal_round_trips() {
        let dir = temp_dir("journal-rt");
        write_journal(&dir, 4, TARGET, 99).unwrap();
        let body = std::fs::read_to_string(dir.join(JOURNAL)).unwrap();
        assert_eq!(parse_journal(&body), Some((4, TARGET, 99)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Crash right after the commit point: sources untouched, full
    /// `.reshard` set + journal on disk. Recovery must back up every
    /// source, finalize the temps, record the layout, drop the journal.
    #[test]
    fn recover_completes_after_commit_point_crash() {
        let dir = temp_dir("mid-c");
        for f in ["dump-0.rdb", "dump-1.rdb", "dump-2.rdb", "dump-3.rdb", "aof-0.aof", "aof-3.aof"] {
            touch(&dir, f);
        }
        touch(&dir, "dump-0.rdb.reshard");
        touch(&dir, "dump-1.rdb.reshard");
        write_journal(&dir, 4, TARGET, 7).unwrap();

        recover_journal(&dir).unwrap();

        for f in ["dump-0.rdb", "dump-1.rdb", "dump-2.rdb", "dump-3.rdb", "aof-0.aof", "aof-3.aof"] {
            assert!(dir.join(format!("{f}.premigration.7")).exists(), "{f} not backed up");
        }
        assert!(dir.join("dump-0.rdb").exists() && dir.join("dump-1.rdb").exists());
        assert!(!dir.join("dump-0.rdb.reshard").exists());
        assert!(!dir.join(JOURNAL).exists());
        assert_eq!(read_shards_meta(&dir.join("shards.meta")), Some(TARGET));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Crash mid-finalize: sources already backed up, one temp already
    /// renamed into place, one still pending. Recovery must not mistake
    /// the finalized snapshot for an old source.
    #[test]
    fn recover_resumes_mid_finalize_crash() {
        let dir = temp_dir("mid-d");
        std::fs::write(dir.join("dump-0.rdb"), b"new0").unwrap(); // finalized
        touch(&dir, "dump-1.rdb.reshard"); // pending
        touch(&dir, "dump-2.rdb.premigration.7"); // already backed up
        write_journal(&dir, 3, TARGET, 7).unwrap();

        recover_journal(&dir).unwrap();

        assert_eq!(std::fs::read(dir.join("dump-0.rdb")).unwrap(), b"new0");
        assert!(!dir.join("dump-0.rdb.premigration.7").exists(), "finalized snapshot re-backed-up");
        assert!(dir.join("dump-1.rdb").exists());
        assert!(!dir.join(JOURNAL).exists());
        assert_eq!(read_shards_meta(&dir.join("shards.meta")), Some(TARGET));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A torn journal (crash mid-journal-write) = the commit point was
    /// never reached: old layout intact, journal discarded, nothing moved.
    #[test]
    fn torn_journal_is_discarded() {
        let dir = temp_dir("torn");
        touch(&dir, "dump-0.rdb");
        touch(&dir, "dump-0.rdb.reshard");
        std::fs::write(dir.join(JOURNAL), b"kevy-reshard-journal v1\nstamp=12").unwrap();

        recover_journal(&dir).unwrap();

        assert!(!dir.join(JOURNAL).exists());
        assert!(dir.join("dump-0.rdb").exists());
        assert!(!dir.join("dump-0.rdb.premigration.12").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_journal_is_a_no_op() {
        let dir = temp_dir("none");
        touch(&dir, "dump-0.rdb");
        recover_journal(&dir).unwrap();
        assert!(dir.join("dump-0.rdb").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

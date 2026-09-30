//! Crash-safe shard-layout migration — the shared engine behind the server
//! runtime's and the embedded store's re-shard paths.
//!
//! Per-shard `dump-{i}.rdb` / `aof-{i}.aof` files are only readable under
//! the shard count (and routing scheme) that wrote them, so changing either
//! re-homes every key: merge every source file into one temp store
//! ([`merge_sources`]), redistribute under the new layout (caller-side — the
//! routing function is the caller's), then commit (`commit_reshard`).
//!
//! Crash-safe ordering (a rename-sources-first order loses the whole
//! keyspace to a crash between the renames and the new writes): new
//! snapshots land under temp `.reshard` names first, then a durable journal
//! marks the commit point, and only then are sources renamed away and the
//! temps finalized. A crash before the journal leaves the old layout fully
//! intact; a crash after it is completed by [`recover_journal`] on the next
//! start. AOFs are not rewritten — each new snapshot is its shard's full
//! state and a fresh (empty) log opens on bring-up; the old logs live on in
//! the `.premigration.<stamp>` backups.
//!
//! ```
//! use kevy_persist::reshard::{StdLayout, commit_reshard, merge_sources, recover_journal};
//! use kevy_persist::{Routing, ShardsMeta, layout, save_snapshot};
//! use kevy_store::{SetCondition, Store};
//!
//! let dir = kevy_tmpdir::unique_dir("reshard-doc");
//! for i in 0..2 {
//!     let mut shard = Store::new();
//!     shard.set(format!("k{i}").as_bytes(), b"v".to_vec(), None, SetCondition::Always);
//!     save_snapshot(&shard, &layout::snapshot_path(&dir, i))?;
//! }
//! recover_journal(&dir, &StdLayout)?; // startup: finish any interrupted reshard first
//! // two shards become one
//! let mut merged = Store::new();
//! merge_sources(&dir, 2, &StdLayout, &mut merged, |_, _| {})?;
//! commit_reshard(&dir, 2, ShardsMeta::new(1, Routing::KevyHash), &[merged], None, &StdLayout)?;
//! assert_eq!(layout::infer_files_n(&dir), 1);
//! # std::fs::remove_dir_all(&dir)?;
//! # Ok::<(), std::io::Error>(())
//! ```

// Best-effort removal, on paths where the file is being abandoned.
// A file that will not delete is a stray the next sweep collects,
// and refusing here would abandon the rest of the cleanup.
#![expect(clippy::let_underscore_must_use, reason = "removing what is already meant to be gone")]

use crate::layout;
use crate::{Argv, ShardsMeta, replay_aof, save_snapshot};
use kevy_store::Store;
use std::io;
use std::path::{Path, PathBuf};

/// Where a layout keeps shard `i`-of-`n`'s files. The standard layout
/// ([`StdLayout`]) is the per-shard names from [`crate::layout`]; the
/// embedded store substitutes its configured single-file names at `n == 1`.
///
/// Recovery resolves paths through the layout *currently in effect* — a
/// journal left by a crash is rolled forward under the caller's present
/// file-name configuration, which must match the one that wrote it.
///
/// Implementations are the host's: each must be a pure function of its
/// arguments (recovery re-derives every path from them after a crash), and
/// must give distinct shards of one layout distinct paths (two shards
/// sharing a file would overwrite each other's snapshot at commit).
///
/// ```
/// use kevy_persist::reshard::{ShardLayout, recover_journal};
/// use std::path::{Path, PathBuf};
///
/// // one shard keeps the host's own file names; more use the standard ones
/// struct Named;
/// impl ShardLayout for Named {
///     fn snapshot_path(&self, dir: &Path, i: usize, n: usize) -> PathBuf {
///         if n == 1 { dir.join("app.rdb") } else { kevy_persist::layout::snapshot_path(dir, i) }
///     }
///     fn aof_path(&self, dir: &Path, i: usize, n: usize) -> PathBuf {
///         if n == 1 { dir.join("app.aof") } else { kevy_persist::layout::aof_path(dir, i) }
///     }
/// }
/// let dir = kevy_tmpdir::unique_dir("shard-layout-doc");
/// assert_eq!(Named.aof_path(&dir, 0, 1), dir.join("app.aof"));
/// recover_journal(&dir, &Named)?; // no journal: nothing to roll forward
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub trait ShardLayout {
    /// Shard `i`'s snapshot path under an `n`-shard layout.
    ///
    /// ```
    /// use kevy_persist::reshard::{ShardLayout, StdLayout};
    /// use std::path::Path;
    ///
    /// let p = StdLayout.snapshot_path(Path::new("/data"), 1, 4);
    /// assert_eq!(p, Path::new("/data/dump-1.rdb"));
    /// ```
    fn snapshot_path(&self, dir: &Path, i: usize, n: usize) -> PathBuf;
    /// Shard `i`'s AOF path under an `n`-shard layout.
    ///
    /// ```
    /// use kevy_persist::reshard::{ShardLayout, StdLayout};
    /// use std::path::Path;
    ///
    /// let p = StdLayout.aof_path(Path::new("/data"), 1, 4);
    /// assert_eq!(p, Path::new("/data/aof-1.aof"));
    /// ```
    fn aof_path(&self, dir: &Path, i: usize, n: usize) -> PathBuf;
}

/// The standard per-shard file names, for every shard count.
///
/// ```
/// use kevy_persist::layout;
/// use kevy_persist::reshard::{ShardLayout, StdLayout};
/// use std::path::Path;
///
/// let dir = Path::new("/data");
/// // the same names whatever the shard count
/// assert_eq!(StdLayout.aof_path(dir, 0, 1), layout::aof_path(dir, 0));
/// assert_eq!(StdLayout.aof_path(dir, 0, 8), layout::aof_path(dir, 0));
/// ```
#[derive(Debug)]
pub struct StdLayout;

impl ShardLayout for StdLayout {
    fn snapshot_path(&self, dir: &Path, i: usize, _n: usize) -> PathBuf {
        layout::snapshot_path(dir, i)
    }
    fn aof_path(&self, dir: &Path, i: usize, _n: usize) -> PathBuf {
        layout::aof_path(dir, i)
    }
}

use crate::reshard_journal::{JOURNAL, parse_journal, write_journal};

/// A target snapshot's temp name: written here first, renamed into place
/// only after the journal commits.
fn reshard_tmp(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".reshard");
    PathBuf::from(s)
}

/// Merge every `src_n`-layout source file in `dir` into `temp`: snapshots
/// load directly, AOF frames go through `replay` (the caller applies them
/// with its own command set). Returns the source paths found — they stay in
/// place; `commit_reshard` backs them up.
///
/// ```
/// use kevy_persist::reshard::{StdLayout, commit_reshard, merge_sources};
/// use kevy_persist::{Routing, ShardsMeta, layout, save_snapshot};
/// use kevy_store::{SetCondition, Store};
///
/// let dir = kevy_tmpdir::unique_dir("merge-doc");
/// for (i, key) in [b"a", b"b"].into_iter().enumerate() {
///     let mut shard = Store::new();
///     shard.set(key, b"v".to_vec(), None, SetCondition::Always);
///     save_snapshot(&shard, &layout::snapshot_path(&dir, i))?;
/// }
/// let mut temp = Store::new();
/// let sources = merge_sources(&dir, 2, &StdLayout, &mut temp, |_, _| {})?;
/// assert_eq!(sources.len(), 2);
/// assert_eq!(temp.dbsize(), 2);
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn merge_sources<L: ShardLayout>(
    dir: &Path,
    src_n: usize,
    lay: &L,
    temp: &mut Store,
    mut replay: impl FnMut(&mut Store, Argv),
) -> io::Result<Vec<PathBuf>> {
    let mut sources: Vec<PathBuf> = Vec::new();
    for i in 0..src_n {
        let snap = lay.snapshot_path(dir, i, src_n);
        let aof = lay.aof_path(dir, i, src_n);
        let log = aof.exists().then_some(aof.as_path());
        if crate::settle_snapshot(&snap, log)? {
            let file = std::io::BufReader::new(std::fs::File::open(&snap)?);
            // the frame kept beside the keyspace goes the way of a logged one
            if let Some(aux) = crate::load_snapshot_with_aux(temp, file, |_| true)? {
                replay(temp, aux);
            }
        }
        if snap.exists() {
            // a snapshot a complete log supersedes is still an old source
            sources.push(snap);
        }
        if aof.exists() {
            replay_aof(&aof, |args| replay(temp, args))?;
            sources.push(aof);
        }
    }
    Ok(sources)
}

/// Commit the redistributed `stores` as the new `target` layout: write each
/// as a `.reshard` temp snapshot, journal the commit point (durable), then
/// finalize — back every `prev_n`-layout source up as
/// `.premigration.<stamp>`, move the temps into place, record the layout,
/// drop the journal. Returns the backup stamp.
///
/// ```
/// use kevy_persist::reshard::{StdLayout, commit_reshard, merge_sources};
/// use kevy_persist::{Routing, ShardsMeta, layout, save_snapshot};
/// use kevy_store::{SetCondition, Store};
///
/// let dir = kevy_tmpdir::unique_dir("commit-doc");
/// for (i, key) in [b"a", b"b"].into_iter().enumerate() {
///     let mut shard = Store::new();
///     shard.set(key, b"v".to_vec(), None, SetCondition::Always);
///     save_snapshot(&shard, &layout::snapshot_path(&dir, i))?;
/// }
/// let mut temp = Store::new();
/// merge_sources(&dir, 2, &StdLayout, &mut temp, |_, _| {})?;
/// let target = ShardsMeta::new(1, Routing::KevyHash);
/// let stamp = commit_reshard(&dir, 2, target, &[temp], None, &StdLayout)?;
/// assert_eq!(ShardsMeta::read(&layout::shards_meta_path(&dir)), Some(target));
/// let backup = format!("dump-1.rdb.premigration.{stamp}");
/// assert!(dir.join(backup).exists(), "the old layout is kept aside");
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn commit_reshard<L: ShardLayout>(
    dir: &Path,
    prev_n: usize,
    target: ShardsMeta,
    stores: &[Store],
    aux: Option<&Argv>,
    lay: &L,
) -> io::Result<u128> {
    // Stale `.reshard` temps from a pre-journal crash are dead weight —
    // the journal was never written, so that attempt never committed.
    for i in 0..target.n {
        let _ = std::fs::remove_file(reshard_tmp(&lay.snapshot_path(dir, i, target.n)));
    }
    for (i, store) in stores.iter().enumerate() {
        let src = crate::WithAux::new(store, aux);
        save_snapshot(&src, &reshard_tmp(&lay.snapshot_path(dir, i, target.n)))?;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    write_journal(dir, prev_n, target, stamp)?; // ── commit point ──
    finish_reshard(dir, prev_n, target, stamp, lay)?;
    Ok(stamp)
}

/// The post-journal half of a reshard: rename every old-layout source to
/// its `.premigration.<stamp>` backup, move the `.reshard` snapshots into
/// place, record the new layout, drop the journal. Every step is an
/// idempotent rename/remove, so this can resume after a crash at any point.
fn finish_reshard<L: ShardLayout>(
    dir: &Path,
    prev_n: usize,
    target: ShardsMeta,
    stamp: u128,
    lay: &L,
) -> io::Result<()> {
    for i in 0..prev_n {
        let snap = lay.snapshot_path(dir, i, prev_n);
        // A plain dump file is an old source unless the new layout already
        // finalized this index (its `.reshard` temp is gone and i < n).
        let is_source = i >= target.n || reshard_tmp(&lay.snapshot_path(dir, i, target.n)).exists();
        if is_source && snap.exists() {
            rename_to_backup(&snap, stamp)?;
        }
        let aof = lay.aof_path(dir, i, prev_n);
        if aof.exists() {
            // Resharded layouts never carry AOFs (fresh logs open on
            // bring-up), so any AOF here is an old source.
            rename_to_backup(&aof, stamp)?;
        }
    }
    for i in 0..target.n {
        let dst = lay.snapshot_path(dir, i, target.n);
        let tmp = reshard_tmp(&dst);
        if tmp.exists() {
            std::fs::rename(&tmp, &dst)?;
        }
    }
    target.write(&layout::shards_meta_path(dir))?;
    std::fs::remove_file(dir.join(JOURNAL))
}

fn rename_to_backup(src: &Path, stamp: u128) -> io::Result<()> {
    let mut bak = src.as_os_str().to_owned();
    bak.push(format!(".premigration.{stamp}"));
    std::fs::rename(src, &bak)
}

/// Startup half of the crash story: a journal on disk means a reshard
/// committed but didn't finish — roll it forward via `finish_reshard`.
/// An unparsable journal means the crash hit mid-journal-write, i.e. the
/// commit point was never reached: the old layout is fully intact, so the
/// torn journal (and any `.reshard` temps, cleaned by the next
/// `commit_reshard`) is safely discarded.
///
/// ```
/// use kevy_persist::reshard::{StdLayout, recover_journal};
///
/// let dir = kevy_tmpdir::unique_dir("recover-doc");
/// // a crash mid-journal-write leaves a torn journal: the commit never happened
/// std::fs::write(dir.join("reshard.journal"), "kevy-resh")?;
/// recover_journal(&dir, &StdLayout)?;
/// assert!(!dir.join("reshard.journal").exists());
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn recover_journal<L: ShardLayout>(dir: &Path, lay: &L) -> io::Result<()> {
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
            finish_reshard(dir, prev_n, target, stamp, lay)?;
            eprintln!(
                "kevy: completed interrupted re-shard to {} shards ({:?} routing)",
                target.n, target.routing,
            );
            Ok(())
        }
        None => std::fs::remove_file(&path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Routing, load_snapshot};

    fn temp_dir(name: &str) -> PathBuf {
        kevy_tmpdir::unique_dir(&format!("reshard-{name}"))
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
        for f in ["dump-0.rdb", "dump-1.rdb", "dump-2.rdb", "dump-3.rdb", "aof-0.aof", "aof-3.aof"]
        {
            touch(&dir, f);
        }
        touch(&dir, "dump-0.rdb.reshard");
        touch(&dir, "dump-1.rdb.reshard");
        write_journal(&dir, 4, TARGET, 7).unwrap();

        recover_journal(&dir, &StdLayout).unwrap();

        for f in ["dump-0.rdb", "dump-1.rdb", "dump-2.rdb", "dump-3.rdb", "aof-0.aof", "aof-3.aof"]
        {
            assert!(dir.join(format!("{f}.premigration.7")).exists(), "{f} not backed up");
        }
        assert!(dir.join("dump-0.rdb").exists() && dir.join("dump-1.rdb").exists());
        assert!(!dir.join("dump-0.rdb.reshard").exists());
        assert!(!dir.join(JOURNAL).exists());
        assert_eq!(ShardsMeta::read(&dir.join("shards.meta")), Some(TARGET));
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

        recover_journal(&dir, &StdLayout).unwrap();

        assert_eq!(std::fs::read(dir.join("dump-0.rdb")).unwrap(), b"new0");
        assert!(!dir.join("dump-0.rdb.premigration.7").exists(), "finalized snapshot re-backed-up");
        assert!(dir.join("dump-1.rdb").exists());
        assert!(!dir.join(JOURNAL).exists());
        assert_eq!(ShardsMeta::read(&dir.join("shards.meta")), Some(TARGET));
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

        recover_journal(&dir, &StdLayout).unwrap();

        assert!(!dir.join(JOURNAL).exists());
        assert!(dir.join("dump-0.rdb").exists());
        assert!(!dir.join("dump-0.rdb.premigration.12").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_journal_is_a_no_op() {
        let dir = temp_dir("none");
        touch(&dir, "dump-0.rdb");
        recover_journal(&dir, &StdLayout).unwrap();
        assert!(dir.join("dump-0.rdb").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A custom single-file layout (the embedded store's filename knobs)
    /// resolves sources and targets through the caller's `ShardLayout`,
    /// including journal recovery.
    #[test]
    fn custom_layout_round_trip() {
        struct Custom;
        impl ShardLayout for Custom {
            fn snapshot_path(&self, dir: &Path, i: usize, n: usize) -> PathBuf {
                if n == 1 { dir.join("snap.bin") } else { layout::snapshot_path(dir, i) }
            }
            fn aof_path(&self, dir: &Path, i: usize, n: usize) -> PathBuf {
                if n == 1 { dir.join("log.bin") } else { layout::aof_path(dir, i) }
            }
        }
        let dir = temp_dir("custom");
        // Shrink 2 → 1 under custom names: merge, commit, verify the
        // custom-named snapshot landed and the std-named sources moved.
        let mut a = Store::new();
        a.set(b"alpha", b"1".to_vec(), None, kevy_store::SetCondition::Always);
        save_snapshot(&a, &dir.join("dump-0.rdb")).unwrap();
        let mut b = Store::new();
        b.set(b"beta", b"2".to_vec(), None, kevy_store::SetCondition::Always);
        save_snapshot(&b, &dir.join("dump-1.rdb")).unwrap();

        let mut temp = Store::new();
        let sources = merge_sources(&dir, 2, &Custom, &mut temp, |_, _| {}).unwrap();
        assert_eq!(sources.len(), 2);
        let target = ShardsMeta { n: 1, routing: Routing::KevyHash };
        let stamp = commit_reshard(&dir, 2, target, &[temp], None, &Custom).unwrap();

        assert!(dir.join("snap.bin").exists());
        assert!(dir.join(format!("dump-0.rdb.premigration.{stamp}")).exists());
        assert!(dir.join(format!("dump-1.rdb.premigration.{stamp}")).exists());
        assert!(!dir.join(JOURNAL).exists());
        let mut merged = Store::new();
        load_snapshot(&mut merged, &dir.join("snap.bin")).unwrap();
        assert_eq!(merged.dbsize(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

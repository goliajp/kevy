//! Append-only command log. Split out from `lib.rs` to keep that file
//! under the 500-LOC house rule; the snapshot writer/reader stays there.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kevy_resp::ArgvView;
use kevy_store::Store;

use crate::{
    dump_store_to_aof, dump_store_to_buf, estimate_multibulk_bytes, write_multibulk,
};

/// 9-byte file-format header written at the start of every kevy-managed
/// AOF as of v1.2.0. `replay_aof` strips it before parsing RESP, so
/// non-kevy bytes accidentally written into the AOF path (e.g. a deploy
/// pipeline redirecting shell stderr into the file) get the same loud
/// rejection as any other corrupt frame. Pre-1.2 AOFs (no magic) still
/// replay — the parser only consumes the magic if it sees it.
pub(crate) const AOF_MAGIC: &[u8; 9] = b"KEVYAOF1\n";

/// When to fsync the AOF to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fsync {
    /// fsync after every write — safest, slowest.
    Always,
    /// fsync at most once per second (call [`Aof::maybe_sync`] periodically).
    EverySec,
    /// Never fsync explicitly; leave it to the OS.
    No,
}

/// An append-only command log. Each write command is appended as a RESP
/// multi-bulk frame; [`crate::replay_aof`] re-applies them on startup.
///
/// Durability model (paired with snapshots): a snapshot taken at T0 plus
/// the AOF of writes in (T0, now] reconstructs the current state. `SAVE`
/// writes the snapshot then [`Aof::truncate`]s the log, so replay never
/// double-applies.
///
/// Sizes (`size_bytes`, `size_at_last_rewrite`) drive auto-trigger of
/// [`Aof::rewrite_from`] (BGREWRITEAOF) via the
/// `auto_aof_rewrite_percentage` + `auto_aof_rewrite_min_size` knobs in
/// `kevy_config`.
pub struct Aof {
    file: BufWriter<File>,
    path: PathBuf,
    fsync: Fsync,
    dirty: bool,
    last_sync: Instant,
    /// Estimated bytes currently in the AOF file (existing + appended since
    /// open). Maintained without fstat() syscalls per append.
    size_bytes: u64,
    /// File size right after the most recent [`Self::rewrite_from`] (or
    /// `Self::open` if never rewritten). Anchor for `auto_aof_rewrite_*`.
    size_at_last_rewrite: u64,
    /// Total rewrites successfully completed since open. Surfaced via INFO.
    rewrites_total: u64,
    /// Group-commit window: while `true`, an `Fsync::Always` `append` only
    /// buffers (sets `dirty`) instead of fsyncing per command. The caller
    /// brackets a batch of writes with [`Self::begin_group`] /
    /// [`Self::end_group`] and `end_group` does the single fsync **before**
    /// the batch's replies are sent — preserving "durable before reply"
    /// while amortizing the per-command `flush()+sync_data()` syscalls.
    /// Only the multi-command reactor entry points (pipelined socket reads,
    /// cross-shard request batches) open a group; every other path keeps
    /// the per-command fsync, so the default is always the safe one.
    deferred: bool,
    /// Non-blocking rewrite "diff buffer". While `Some`, every `append` also
    /// tees its RESP frame here, so writes that land *during* an off-lock
    /// rewrite are captured and replayed after the compacted snapshot. See
    /// [`Self::begin_concurrent_rewrite`].
    rewrite_tee: Option<Vec<u8>>,
}

/// Handoff between the two halves of a non-blocking rewrite: the serialized
/// keyspace image (produced under the store lock) and the temp path to spill
/// it to (off-lock). See [`Aof::begin_concurrent_rewrite`].
pub struct RewritePlan {
    /// The compacted AOF image (magic + one command stream per key).
    pub body: Vec<u8>,
    /// Same-directory temp file to spill `body` to before the final swap.
    pub tmp: PathBuf,
    /// Keys captured in `body` (for the resulting [`RewriteStats`]).
    pub keys: u64,
}

/// Result of an [`Aof::rewrite_from`] call. Surfaced by `BGREWRITEAOF` /
/// `INFO persistence`.
#[derive(Debug, Clone, Copy)]
pub struct RewriteStats {
    /// Keys dumped into the new AOF.
    pub keys: u64,
    /// New AOF size in bytes.
    pub bytes: u64,
}

impl Aof {
    /// Open (creating if needed) `path` for appending. New files get the
    /// 9-byte `AOF_MAGIC` header so replays can identify the file as
    /// kevy-managed. Pre-existing files (legacy bare-RESP or already-
    /// magic'd) are left untouched.
    pub fn open(path: &Path, fsync: Fsync) -> io::Result<Self> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut size = file.metadata().map(|m| m.len()).unwrap_or(0);
        if size == 0 {
            // Fresh file: stamp the magic header so the replayer can
            // distinguish kevy-written AOFs from accidental writes.
            file.write_all(AOF_MAGIC)?;
            file.sync_data()?;
            size = AOF_MAGIC.len() as u64;
        }
        Ok(Aof {
            file: BufWriter::new(file),
            path: path.to_path_buf(),
            fsync,
            dirty: false,
            last_sync: Instant::now(),
            size_bytes: size,
            size_at_last_rewrite: size,
            rewrites_total: 0,
            deferred: false,
            rewrite_tee: None,
        })
    }

    /// The fsync policy this AOF was opened with (or last switched to).
    /// Mostly for tests / INFO output; the hot path doesn't read this.
    #[inline]
    pub fn fsync_policy(&self) -> Fsync {
        self.fsync
    }

    /// Switch the fsync policy at runtime (called by `CONFIG SET
    /// appendfsync`). When tightening to `Always`, also flushes + fsyncs
    /// any bytes still in the BufWriter so the new "every write is on
    /// disk before reply" contract is honoured starting on the next
    /// append, not after the dirty backlog clears.
    pub fn set_fsync(&mut self, fsync: Fsync) -> io::Result<()> {
        let upgrading_to_always = matches!(fsync, Fsync::Always) && !matches!(self.fsync, Fsync::Always);
        self.fsync = fsync;
        if upgrading_to_always && self.dirty {
            self.file.flush()?;
            self.file.get_ref().sync_data()?;
            self.dirty = false;
            self.last_sync = Instant::now();
        }
        Ok(())
    }

    /// Append one command, applying the fsync policy.
    pub fn append<A: ArgvView + ?Sized>(&mut self, args: &A) -> io::Result<()> {
        write_multibulk(&mut self.file, args)?;
        // Tee into the in-flight rewrite's diff buffer (off-lock rewrite in
        // progress): re-encode the same frame so it survives the swap. Only
        // active during the rare rewrite window — zero cost otherwise.
        if let Some(tee) = &mut self.rewrite_tee {
            write_multibulk(tee, args)?;
        }
        self.size_bytes = self
            .size_bytes
            .saturating_add(estimate_multibulk_bytes(args));
        match self.fsync {
            // Inside a group-commit window, defer the fsync to `end_group`
            // (one per batch, still before the batch's replies). Outside
            // one, fsync per command — the safe default for every path.
            Fsync::Always if self.deferred => self.dirty = true,
            Fsync::Always => {
                self.file.flush()?;
                self.file.get_ref().sync_data()?;
            }
            Fsync::EverySec | Fsync::No => self.dirty = true,
        }
        Ok(())
    }

    /// Open a group-commit window (no-op unless the policy is `Always`):
    /// subsequent `append`s buffer instead of fsyncing per command. Pair
    /// with [`Self::end_group`] **before** sending the batch's replies.
    #[inline]
    pub fn begin_group(&mut self) {
        if matches!(self.fsync, Fsync::Always) {
            self.deferred = true;
        }
    }

    /// Close the group-commit window: one `flush()+sync_data()` for the
    /// whole batch (if anything was buffered), then resume per-command
    /// fsync. Must be called before the batch's replies leave the shard.
    #[inline]
    pub fn end_group(&mut self) -> io::Result<()> {
        if self.deferred {
            self.deferred = false;
            if self.dirty {
                self.file.flush()?;
                self.file.get_ref().sync_data()?;
                self.dirty = false;
                self.last_sync = Instant::now();
            }
        }
        Ok(())
    }

    /// Flush+fsync if the `EverySec` window has elapsed. Call once per loop tick.
    pub fn maybe_sync(&mut self) -> io::Result<()> {
        if matches!(self.fsync, Fsync::EverySec)
            && self.dirty
            && self.last_sync.elapsed() >= Duration::from_secs(1)
        {
            self.file.flush()?;
            self.file.get_ref().sync_data()?;
            self.dirty = false;
            self.last_sync = Instant::now();
        }
        Ok(())
    }

    /// Empty the log (after a snapshot has captured the full state). The
    /// post-truncate file keeps the `AOF_MAGIC` header so replays of
    /// the freshly-trimmed log still identify as kevy-managed.
    pub fn truncate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let f = self.file.get_mut();
        f.set_len(0)?;
        f.seek(SeekFrom::Start(0))?; // harmless under O_APPEND; keeps len/pos coherent
        f.write_all(AOF_MAGIC)?;
        f.sync_all()?;
        self.dirty = false;
        self.size_bytes = AOF_MAGIC.len() as u64;
        self.size_at_last_rewrite = AOF_MAGIC.len() as u64;
        Ok(())
    }

    /// Estimated current AOF size in bytes (file content as of last append).
    #[inline]
    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    /// AOF size at the most recent rewrite (or open). Auto-trigger compares
    /// `(size_bytes - size_at_last_rewrite) * 100 / size_at_last_rewrite` to
    /// the `auto_aof_rewrite_percentage` knob.
    #[inline]
    pub fn size_at_last_rewrite(&self) -> u64 {
        self.size_at_last_rewrite
    }

    /// Successful rewrite count since `Self::open`. Surfaced in INFO.
    #[inline]
    pub fn rewrites_total(&self) -> u64 {
        self.rewrites_total
    }

    /// BGREWRITEAOF: rebuild a compact AOF from `store`'s current state and
    /// atomically swap it in.
    ///
    /// **v1.0 is synchronous** — the calling shard blocks for the rewrite's
    /// duration. Each shard owns its own AOF, so the shards' rewrites
    /// proceed independently; per-shard blocking matches Redis's `BGSAVE`
    /// cost in a typical single-key-per-shard workload. Concurrent
    /// (rewrite-during-writes) incrementalisation is a v1.x perf item.
    ///
    /// Writes to a `<path>.rewrite` temp file with fsync, then `rename(2)`s
    /// it over the live AOF. The append handle is reopened against the new
    /// file before this call returns, so subsequent `append` calls land in
    /// the rewritten log.
    pub fn rewrite_from(&mut self, store: &Store) -> io::Result<RewriteStats> {
        // Flush any pending writes to the OLD file first so the snapshot
        // accounts for everything the caller intended to durabilise.
        self.file.flush()?;

        let tmp = rewrite_tmp_path(&self.path);
        let (keys, bytes) = dump_store_to_aof(&tmp, store)?;

        // Atomic replacement. After this, the OLD file descriptor in
        // `self.file` is open against an unlinked inode; new writes would
        // go nowhere visible. Reopen against the new path.
        std::fs::rename(&tmp, &self.path)?;
        let f = OpenOptions::new().append(true).open(&self.path)?;
        self.file = BufWriter::new(f);
        self.size_bytes = bytes;
        self.size_at_last_rewrite = bytes;
        self.dirty = false;
        self.rewrites_total = self.rewrites_total.saturating_add(1);
        Ok(RewriteStats { keys, bytes })
    }

    /// Is a non-blocking rewrite mid-flight (between
    /// [`Self::begin_concurrent_rewrite`] and `finish`/`abort`)? While true,
    /// don't start another rewrite — `append` is teeing into the diff buffer.
    #[inline]
    pub fn is_rewriting(&self) -> bool {
        self.rewrite_tee.is_some()
    }

    /// Phase 1 of a **non-blocking** rewrite (Background auto-rewrite). Must be
    /// called under the store lock: it serializes the keyspace into an
    /// in-memory image and starts teeing subsequent `append`s into a diff
    /// buffer — both atomic w.r.t. other writes. The caller then spills
    /// `plan.body` to `plan.tmp` **with the lock released** (the slow disk
    /// write), and finally calls [`Self::finish_concurrent_rewrite`] under the
    /// lock again. Writes that land during the off-lock spill are captured by
    /// the tee and appended after the snapshot, so nothing is lost.
    pub fn begin_concurrent_rewrite(&mut self, store: &Store) -> io::Result<RewritePlan> {
        let (body, keys) = dump_store_to_buf(store);
        self.rewrite_tee = Some(Vec::new());
        Ok(RewritePlan {
            body,
            tmp: rewrite_tmp_path(&self.path),
            keys,
        })
    }

    /// Phase 2: the `plan.body` is already on disk at `tmp` (spilled off-lock).
    /// Append the diff buffer (writes since `begin`), fsync, atomically swap
    /// over the live AOF, and reopen the append handle against it. Call under
    /// the store lock. `keys` is `plan.keys`.
    pub fn finish_concurrent_rewrite(&mut self, tmp: &Path, keys: u64) -> io::Result<RewriteStats> {
        let tee = self.rewrite_tee.take().unwrap_or_default();
        {
            let mut f = OpenOptions::new().append(true).open(tmp)?;
            f.write_all(&tee)?;
            f.sync_all()?;
        }
        std::fs::rename(tmp, &self.path)?;
        let f = OpenOptions::new().append(true).open(&self.path)?;
        let bytes = f.metadata().map(|m| m.len()).unwrap_or(0);
        self.file = BufWriter::new(f);
        self.size_bytes = bytes;
        self.size_at_last_rewrite = bytes;
        self.dirty = false;
        self.rewrites_total = self.rewrites_total.saturating_add(1);
        Ok(RewriteStats { keys, bytes })
    }

    /// Abandon an in-flight non-blocking rewrite (e.g. the off-lock spill
    /// failed): drop the diff buffer and resume normal appends. The live AOF
    /// is untouched, so no data is at risk; the caller deletes the temp file.
    pub fn abort_concurrent_rewrite(&mut self) {
        self.rewrite_tee = None;
    }
}

/// `<aof>.rewrite` — same-directory temp path so `rename(2)` stays atomic.
fn rewrite_tmp_path(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    let new_name = match path.file_name() {
        Some(n) => {
            let mut s = n.to_os_string();
            s.push(".rewrite");
            s
        }
        None => std::ffi::OsString::from("aof.rewrite"),
    };
    p.set_file_name(new_name);
    p
}

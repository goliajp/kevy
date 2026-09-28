//! Append-only command log. Split out from `lib.rs` to keep that file
//! under the 500-LOC house rule; the snapshot writer/reader stays there.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use kevy_resp::ArgvView;
use kevy_store::Store;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use crate::aof_mapped::{MapHandle, Mapped, sync_handles};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use crate::aof_stage::Stage;
#[cfg(target_arch = "wasm32")]
pub(crate) use crate::aof_stage_off::{MapHandle, Mapped, Stage, sync_handles};
use crate::estimate_multibulk_bytes;
use crate::record::RECORD_HEADER;
use crate::record_pieces::{record_header, write_frame};

/// 9-byte file-format header written at the start of every kevy-managed
/// AOF. `replay_aof` strips it before parsing RESP, so
/// non-kevy bytes accidentally written into the AOF path (e.g. a deploy
/// pipeline redirecting shell stderr into the file) get the same loud
/// rejection as any other corrupt frame. Legacy AOFs (no magic) still
/// replay — the parser only consumes the magic if it sees it.
///
/// Public so host-mediated AOF sinks (a browser pump appending kevy
/// frames to its own storage, for example) can stamp files that stay
/// byte-compatible with kevy-written logs.
pub const AOF_MAGIC: &[u8; 9] = b"KEVYAOF1\n";

/// AOF write buffer capacity. `BufWriter`'s default is 8 KiB — a single
/// 4 KiB value fills it in two writes, so the append path spends ~half
/// its time in the `write` syscall (perf-measured: SET 4 KiB, 52% in
/// `write`/`ksys_write`, on both tmpfs and ext4). MMKV's mmap append
/// pays no syscall at all; a larger buffer amortises the write across
/// many appends the same way. `No` and `EverySec` empty the buffer on
/// every tick, so its size does not widen the loss window.
/// 256 KiB holds ~64 4 KiB appends per syscall; per-shard cost is one
/// such buffer.
pub(crate) const AOF_BUF_CAP: usize = 256 * 1024;

/// When to fsync the AOF to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fsync {
    /// fsync after every write — safest, slowest.
    Always,
    /// fsync about once per second; each [`Aof::tick`] (or
    /// [`Aof::maybe_sync`]) writes the buffer into the kernel.
    EverySec,
    /// Never fsync explicitly: each [`Aof::tick`] writes the buffer into
    /// the kernel, and the OS decides when it reaches the disk.
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
#[derive(Debug)]
pub struct Aof {
    pub(crate) file: BufWriter<File>,
    /// A begin marker has been written and its commit marker has not.
    pub(crate) in_txn: bool,
    pub(crate) path: PathBuf,
    pub(crate) fsync: Fsync,
    pub(crate) dirty: bool,
    pub(crate) last_sync: Instant,
    /// `EverySec` syncs handed out by `tick`, and the highest one whose
    /// fsync completed (shared with the handed-out [`crate::PendingSync`]).
    pub(crate) sync_started: u64,
    pub(crate) sync_confirmed: Arc<AtomicU64>,
    /// Estimated bytes currently in the AOF file (existing + appended since
    /// open). Maintained without fstat() syscalls per append.
    pub(crate) size_bytes: u64,
    /// File size right after the most recent [`Self::rewrite_from`] (or
    /// `Self::open` if never rewritten). Anchor for `auto_aof_rewrite_*`.
    pub(crate) size_at_last_rewrite: u64,
    /// Total rewrites successfully completed since open. Surfaced via INFO.
    pub(crate) rewrites_total: u64,
    /// Group-commit window: while `true`, an `Fsync::Always` `append` only
    /// buffers (sets `dirty`) instead of fsyncing per command. The caller
    /// brackets a batch of writes with [`Self::begin_group`] /
    /// [`Self::end_group`] and `end_group` does the single fsync **before**
    /// the batch's replies are sent — preserving "durable before reply"
    /// while amortizing the per-command `flush()+sync_data()` syscalls.
    /// Only the multi-command reactor entry points (pipelined socket reads,
    /// cross-shard request batches) open a group; every other path keeps
    /// the per-command fsync, so the default is always the safe one.
    pub(crate) deferred: bool,
    /// Non-blocking rewrite "diff buffer". While `Some`, every `append` also
    /// tees its RESP frame here, so writes that land *during* an off-lock
    /// rewrite are captured and replayed after the compacted snapshot. See
    /// [`Self::begin_concurrent_rewrite`].
    pub(crate) rewrite_tee: Option<Vec<u8>>,
    /// Recycled tee buffer (two-phase rewrite): the worker returns each
    /// appended generation's buffer CLEARED, and the next generation
    /// grows into its warm pages instead of faulting a fresh range —
    /// the reactor-vs-worker mmap churn convicted in the S5-E finding.
    pub(crate) tee_spare: Option<Vec<u8>>,
    /// The pre-swap log's graveyard hardlink, awaiting an off-thread
    /// unlink (see `swap_image` — rename must not drop a multi-GB
    /// inode's last link on the reactor).
    pub(crate) swap_trash: Option<PathBuf>,
    /// Off-thread swap window open: the driver holds queue drains and
    /// fsyncs while the worker renames over the live path.
    pub(crate) swap_hold: bool,
    /// Where `open` quarantined a dropped tail, if it had to repair one —
    /// surfaced so the store's open report can name the file.
    open_quarantine: Option<PathBuf>,
    /// When the last rewrite (or the open, if none yet) finished — the
    /// anchor for [`RewritePolicy::interval_secs`].
    pub(crate) last_rewrite_at: Instant,
    /// The on-disk encoding this file speaks. New files and every rewrite
    /// output are V2 (checksummed record envelopes); a pre-existing V1
    /// file keeps appending V1 until its first rewrite upgrades it —
    /// mixing formats within one file would corrupt it.
    pub(crate) format: crate::AofFormat,
    /// `Some` = staged appends (see `aof_stage`): records land in a shared
    /// mapping first and reach `file` on each drain.
    pub(crate) stage: Option<Stage>,
    /// `Some` = mapped appends (see `aof_mapped`): records are copied into a
    /// mapping of the file's preallocated tail; `file` is never written.
    pub(crate) mapped: Option<Mapped>,
    /// This log maps its appends whenever it can: kept while the mapping is
    /// taken down around a file swap, so the swap maps the new file.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) maps: bool,
    /// `Some` = queued-append mode (RFC v3-aof-offload S1): encoded
    /// record bytes accumulate here instead of hitting `file`, and the
    /// DRIVER (the io_uring reactor) drains them via
    /// [`Aof::take_pending`] as async write SQEs at
    /// [`Aof::append_offset`]. `None` = every append writes
    /// synchronously — today's behavior, and the epoll / test default.
    ///
    /// Contract for the driver: before any structural file operation
    /// (truncate, rewrite finish, fsync-policy upgrade to Always) the
    /// driver must have COMPLETED its in-flight writes; bytes still
    /// queued HERE are flushed synchronously by those entry points as
    /// an honest fallback, but bytes already taken are invisible to
    /// this struct and only the driver can order them.
    pub(crate) queue: Option<Vec<u8>>,
    /// File offset where the NEXT taken chunk lands (queued mode):
    /// advances as chunks are taken, so concurrent chunks carry
    /// non-overlapping explicit offsets in their SQEs.
    pub(crate) queued_offset: u64,
    /// Monotone count of records ever pushed into `queue`. The uring
    /// driver's Always reply-gate (S2) compares this watermark against
    /// the fsync-proven durable watermark; unlike file offsets it never
    /// resets across a rewrite swap, so held replies cannot wedge.
    pub(crate) queued_seq: u64,
}

/// Handoff between the two halves of a non-blocking rewrite: the serialized
/// keyspace image (produced under the store lock) and the temp path to spill
/// it to (off-lock). See [`Aof::begin_concurrent_rewrite`].
#[derive(Debug)]
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
    /// The on-disk record format this file currently speaks.
    ///
    /// A `V1` answer means a 3.x binary can still open this file — the
    /// downgrade window `UPGRADING.md` describes is a *state*, and this
    /// is where an embedder reads it instead of telling their users
    /// "assume it closed" (an embedder's dogfood ask: their `doctor`
    /// command wanted to say "you can still swap the binary back" and
    /// could not, because this was `pub(crate)`).
    #[must_use]
    pub fn format(&self) -> crate::AofFormat {
        self.format
    }

    /// Open (creating if needed) `path` for appending. New files get the
    /// 9-byte `AOF_MAGIC` header so replays can identify the file as
    /// kevy-managed. Pre-existing files (legacy bare-RESP or already-
    /// magic'd) are left untouched.
    pub fn open(path: &Path, fsync: Fsync) -> io::Result<Self> {
        Self::open_with_repair(path, fsync, false)
    }

    /// [`Self::open`] with the repair policy explicit: under `resync`,
    /// interior corrupt regions are left in place (the resync replay hops
    /// them deterministically each boot until a rewrite compacts them
    /// away) and only the bytes after the LAST recoverable record are
    /// quarantined + truncated — so a mid-file corruption no longer costs
    /// the good tail behind it.
    pub fn open_with_repair(path: &Path, fsync: Fsync, resync: bool) -> io::Result<Self> {
        Self::open_after_replay(path, fsync, resync, None)
    }

    /// [`Self::open_with_repair`] for a file a replay just walked: `settled`
    /// is where it stopped, when it dropped nothing, so anything past it is
    /// a mapped log's unused zero preallocation. A v2 file is cut back to
    /// that length without the second walk the repair would make — with
    /// the same parser, it would stop at the same byte.
    pub fn open_after_replay(
        path: &Path,
        fsync: Fsync,
        resync: bool,
        settled: Option<u64>,
    ) -> io::Result<Self> {
        let (file, size, format, quarantined) = Self::prepare_file(path, resync, settled)?;
        Ok(Aof {
            in_txn: false,
            file: BufWriter::with_capacity(AOF_BUF_CAP, file),
            path: path.to_path_buf(),
            fsync,
            dirty: false,
            last_sync: Instant::now(),
            sync_started: 0,
            sync_confirmed: Arc::new(AtomicU64::new(0)),
            size_bytes: size,
            size_at_last_rewrite: size,
            rewrites_total: 0,
            deferred: false,
            rewrite_tee: None,
            tee_spare: None,
            swap_trash: None,
            swap_hold: false,
            open_quarantine: quarantined,
            last_rewrite_at: Instant::now(),
            format,
            queue: None,
            queued_offset: size,
            queued_seq: 0,
            stage: None,
            mapped: None,
            #[cfg(not(target_arch = "wasm32"))]
            maps: false,
        })
    }

    /// Open `path` for appending: a fresh file gets the v2 magic, an existing
    /// one keeps its format (v1 upgrades at the next rewrite) and has its
    /// tail settled. Returns the file, its length, its format and where a
    /// repaired tail was quarantined.
    fn prepare_file(
        path: &Path,
        resync: bool,
        settled: Option<u64>,
    ) -> io::Result<(File, u64, crate::AofFormat, Option<PathBuf>)> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut size = file.metadata().map_or(0, |m| m.len());
        if size == 0 {
            // stamp the magic so the replayer can tell a kevy-written log
            // from an accidental write
            file.write_all(crate::record::AOF2_MAGIC)?;
            file.sync_data()?;
            size = crate::record::AOF2_MAGIC.len() as u64;
            return Ok((file, size, crate::AofFormat::V2, None));
        }
        let format = crate::replay::sniff_format(path)?;
        let settled = settled.filter(|_| format == crate::AofFormat::V2);
        let quarantined =
            crate::aof_util::settle_tail(path, &mut file, &mut size, settled, resync)?;
        Ok((file, size, format, quarantined))
    }

    /// The quarantine file `open` wrote while repairing a dropped tail, if
    /// any. `None` after a clean open.
    #[inline]
    pub fn open_quarantine(&self) -> Option<&Path> {
        self.open_quarantine.as_deref()
    }

    /// When the last rewrite (or the open) finished — the staleness anchor
    /// [`crate::RewritePolicy`] measures from.
    #[inline]
    pub(crate) fn last_rewrite_at(&self) -> Instant {
        self.last_rewrite_at
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
        let upgrading_to_always =
            matches!(fsync, Fsync::Always) && !matches!(self.fsync, Fsync::Always);
        self.fsync = fsync;
        if upgrading_to_always {
            self.flush_queued()?;
            self.stop_mapping()?;
        }
        if upgrading_to_always && (self.dirty || self.sync_unconfirmed()) {
            self.file.flush()?;
            self.file.get_ref().sync_data()?;
            self.dirty = false;
            self.last_sync = Instant::now();
            self.confirm_started_syncs();
        }
        Ok(())
    }

    /// Append one command, applying the fsync policy. V2 files get the
    /// checksummed record envelope; a V1 file keeps its bare-RESP form
    /// until a rewrite upgrades it.
    pub fn append<A: ArgvView + ?Sized>(&mut self, args: &A) -> io::Result<()> {
        // the frame goes from the caller's slices into each sink directly;
        // the tee is always V2 because the rewrite output it lands in is
        let v2 = matches!(self.format, crate::AofFormat::V2);
        let head = (v2 || self.rewrite_tee.is_some()).then(|| record_header(args));
        let own = if v2 { head.as_ref() } else { None };
        if let Some(q) = &mut self.queue {
            write_frame(q, own, args)?;
            self.queued_seq += 1;
        } else if let Some(m) = &mut self.mapped {
            write_frame(m, own, args)?;
        } else if let Some(h) = own.filter(|_| self.stage.is_some()) {
            let len = RECORD_HEADER + u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
            let staged = self.stage_record(len, |mut slot| {
                write_frame(&mut slot, Some(h), args).expect("the slot is the record's exact size");
            })?;
            if !staged {
                write_frame(&mut self.file, own, args)?;
                self.stage_bypassed()?;
            }
        } else {
            write_frame(&mut self.file, own, args)?;
        }
        if let (Some(tee), Some(h)) = (&mut self.rewrite_tee, &head) {
            write_frame(tee, Some(h), args)?;
        }
        let overhead = match self.format {
            crate::AofFormat::V2 => crate::record::RECORD_HEADER as u64,
            crate::AofFormat::V1 => 0,
        };
        self.size_bytes =
            self.size_bytes.saturating_add(estimate_multibulk_bytes(args)).saturating_add(overhead);
        match self.fsync {
            // Inside a group-commit window, defer the fsync to `end_group`
            // (one per batch, still before the batch's replies). Outside
            // one, fsync per command — the safe default for every path.
            // Queued appends live in the driver's chunk, not the file: a
            // sync here would durabilize nothing. The ring fsync owns
            // durability there; mark dirty so the driver can see it.
            Fsync::Always if self.deferred || self.queue.is_some() => self.dirty = true,
            Fsync::Always => {
                self.file.flush()?;
                self.file.get_ref().sync_data()?;
            }
            Fsync::EverySec | Fsync::No => self.dirty = true,
        }
        Ok(())
    }

    /// Empty the log (after a snapshot has captured the full state). The
    /// post-truncate file keeps the `AOF_MAGIC` header so replays of
    /// the freshly-trimmed log still identify as kevy-managed.
    pub fn truncate(&mut self) -> io::Result<()> {
        self.flush_queued()?;
        self.unmap()?;
        self.file.flush()?;
        let f = self.file.get_mut();
        f.set_len(0)?;
        f.seek(SeekFrom::Start(0))?; // harmless under O_APPEND; keeps len/pos coherent
        f.write_all(crate::record::AOF2_MAGIC)?;
        f.sync_all()?;
        self.dirty = false;
        self.format = crate::AofFormat::V2; // an empty log restarts in v2
        self.size_bytes = crate::record::AOF2_MAGIC.len() as u64;
        self.queued_offset = self.size_bytes;
        self.size_at_last_rewrite = crate::record::AOF2_MAGIC.len() as u64;
        self.last_rewrite_at = Instant::now();
        self.after_file_change()
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

    /// Re-anchor the growth-rule baseline to `bytes` — the live image's
    /// estimated rewrite size ([`crate::estimate_rewrite_size`]), called
    /// by open paths after replay. `open` alone can only baseline at the
    /// file's current size, which for a short-lived process re-opening
    /// the same directory resets the growth ratio every run and lets the
    /// log grow without bound; anchoring to the live estimate keeps the
    /// +pct% rule meaning "the log is pct% history" across processes.
    /// Ignored while a rewrite is in flight (its completion sets the
    /// true post-rewrite size).
    pub fn anchor_rewrite_baseline(&mut self, bytes: u64) {
        if !self.is_rewriting() {
            self.size_at_last_rewrite = bytes.max(crate::record::AOF2_MAGIC.len() as u64);
        }
    }

    /// Successful rewrite count since `Self::open`. Surfaced in INFO.
    #[inline]
    pub fn rewrites_total(&self) -> u64 {
        self.rewrites_total
    }

    /// BGREWRITEAOF: rebuild a compact AOF from `store`'s current state and
    /// atomically swap it in.
    ///
    /// **Synchronous** — the calling shard blocks for the rewrite's
    /// duration. Each shard owns its own AOF, so the shards' rewrites
    /// proceed independently; per-shard blocking matches Redis's `BGSAVE`
    /// cost in a typical single-key-per-shard workload. Concurrent
    /// (rewrite-during-writes) incrementalisation is deliberately not
    /// attempted here.
    ///
    /// Writes to a `<path>.rewrite` temp file with fsync, then `rename(2)`s
    /// it over the live AOF. The append handle is reopened against the new
    /// file before this call returns, so subsequent `append` calls land in
    /// the rewritten log.
    pub fn rewrite_from(&mut self, store: &Store) -> io::Result<RewriteStats> {
        // Flush any pending writes to the OLD file first so the snapshot
        // accounts for everything the caller intended to durabilise.
        self.flush_queued()?;
        self.unmap()?;
        self.file.flush()?;

        let tmp = crate::aof_util::rewrite_tmp_path(&self.path);
        let (keys, bytes) = crate::dump_aof(&tmp, store)?;

        // Atomic replacement. After this, the OLD file descriptor in
        // `self.file` is open against an unlinked inode; new writes would
        // go nowhere visible. Reopen against the new path.
        std::fs::rename(&tmp, &self.path)?;
        let f = OpenOptions::new().append(true).open(&self.path)?;
        self.file = BufWriter::with_capacity(AOF_BUF_CAP, f);
        self.format = crate::AofFormat::V2; // the rewrite output always is
        self.size_bytes = bytes;
        self.queued_offset = bytes;
        self.size_at_last_rewrite = bytes;
        self.last_rewrite_at = Instant::now();
        self.dirty = false;
        self.rewrites_total = self.rewrites_total.saturating_add(1);
        self.after_file_change()?;
        Ok(RewriteStats { keys, bytes })
    }

    /// Is a non-blocking rewrite mid-flight (between
    /// [`Self::begin_concurrent_rewrite`] and `finish`/`abort`)? While true,
    /// don't start another rewrite — `append` is teeing into the diff buffer.
    #[inline]
    pub fn is_rewriting(&self) -> bool {
        self.rewrite_tee.is_some()
    }
}

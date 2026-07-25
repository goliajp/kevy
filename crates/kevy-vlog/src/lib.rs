//! kevy-vlog — the disposable value log under transparent tiering.
//!
//! An append-only spill area for cold VALUES: keys and metadata stay
//! in RAM; a demoted value's bytes live here and are read back with
//! one `read_at`.
//! Three properties are load-bearing:
//!
//! - **Disposable.** The vlog is NOT durability — the AOF remains the sole
//!   durable truth. [`Vlog::open`] deletes any leftover vlog files; every
//!   boot starts empty and refills during replay. There is deliberately no
//!   fsync, no recovery scan, no torn-write healing: a record that fails
//!   its CRC on read-back is a bug in this process, not corruption to
//!   repair, and surfaces as an error.
//! - **Pinned readers.** A reader that must survive compaction (a snapshot
//!   view, an AOF rewrite, a replication ship) holds an [`Arc<VlogFile>`]
//!   from [`Vlog::pin`]. A compacted file is unlinked only when the last
//!   pin drops ([`VlogFile`]'s `Drop`), so a pinned view's offsets stay
//!   valid for its whole life. All file IO is positional
//!   (`read_at`/`write_at`, `&File`), so pinned reads are thread-safe.
//! - **Owner-driven compaction.** Records carry their key, so a file is
//!   self-describing; but only the owner (the store's cold refs) knows
//!   which records are live. [`Vlog::compact_below`] scans, asks the owner
//!   ([`CompactOwner::is_live`]), re-appends survivors, and hands the new
//!   ref back ([`CompactOwner::moved`]). Every compaction bumps
//!   [`Vlog::epoch`] — the O(1) "did my ColdRef move?" check.
//!
//! Record layout: `[body_len u32-LE][crc32c u32-LE][key_len u32-LE][key]
//! [payload]`, CRC over the body (everything after the 8-byte header).
//! A [`VlogRef`] names `(file_id, header offset, body_len)`.

mod crc32c;
use crc32c::crc32c;

#[cfg(test)]
mod tests;

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Default rotation threshold for the active file (RFC §7: 256 MiB).
pub const DEFAULT_ROTATE_BYTES: u64 = 256 << 20;

/// Per-record header: `body_len u32-LE | crc32c u32-LE`.
const HEADER: u64 = 8;

/// Refuse absurd bodies (mirrors the AOF envelope's `MAX_RECORD` bound).
const MAX_BODY: u32 = 1 << 30;

/// The address of one spilled record — what a cold stub holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VlogRef {
    pub file_id: u32,
    /// Byte offset of the record HEADER within the file.
    pub offset: u64,
    /// Body length (key_len field + key + payload), excluding the header.
    pub len: u32,
}

impl VlogRef {
    /// Total on-disk record length: header + body. The image size a
    /// batched reader must fetch at `offset` (see [`verify_image`]).
    #[inline]
    pub fn disk_len(self) -> usize {
        HEADER as usize + self.len as usize
    }
}

/// Verify a raw record image (the `disk_len()` bytes at `r.offset`) and
/// split it into `(key, payload)`. This is the completion half of a
/// batched read: the io_uring path fetches images concurrently and runs
/// each through here; [`VlogFile::read`] is exactly one fetch + this.
/// A length or CRC mismatch is `InvalidData` — this process wrote the
/// record this boot, so a bad image is a bug, never corruption to heal.
pub fn verify_image(r: VlogRef, mut image: Vec<u8>) -> io::Result<(Vec<u8>, Vec<u8>)> {
    if image.len() != r.disk_len() {
        return Err(bad(format!(
            "vlog: image length mismatch (want {}, got {})",
            r.disk_len(),
            image.len()
        )));
    }
    let body_len = u32::from_le_bytes(image[..4].try_into().unwrap());
    let crc = u32::from_le_bytes(image[4..8].try_into().unwrap());
    if body_len != r.len || body_len > MAX_BODY {
        return Err(bad(format!("vlog: length mismatch (ref {}, disk {body_len})", r.len)));
    }
    if crc32c(&image[HEADER as usize..]) != crc {
        return Err(bad(format!("vlog: crc mismatch at {}:{}", r.file_id, r.offset)));
    }
    image.drain(..HEADER as usize);
    split_body(image)
}

/// One log file. Shared via `Arc`: the `Vlog` holds one, and pinned
/// readers hold more. When compaction retires the file it sets
/// `delete_on_drop`; the underlying file is unlinked by whichever holder
/// drops last — that is the entire pin protocol.
pub struct VlogFile {
    id: u32,
    path: PathBuf,
    file: File,
    delete_on_drop: AtomicBool,
}

impl VlogFile {
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Read one record back: `(key, payload)`. ONE positional pread of
    /// the whole record image (the body length is already in the ref),
    /// then [`verify_image`] — a length/CRC mismatch is `InvalidData`
    /// (this process wrote the record this boot; a bad read is a bug,
    /// never "corruption to heal").
    pub fn read(&self, r: VlogRef) -> io::Result<(Vec<u8>, Vec<u8>)> {
        verify_image(r, self.read_image(r)?)
    }

    /// Fetch the raw record image (`r.disk_len()` bytes at `r.offset`)
    /// in one pread, UNverified — the batched-read issuance half; pair
    /// with [`verify_image`] on completion.
    pub fn read_image(&self, r: VlogRef) -> io::Result<Vec<u8>> {
        let mut image = vec![0u8; r.disk_len()];
        self.file.read_exact_at(&mut image, r.offset)?;
        Ok(image)
    }

    /// The underlying file descriptor — what an io_uring batch reader
    /// preps its READ SQEs against. The fd stays valid for the life of
    /// this pin (the whole point of holding the `Arc<VlogFile>`).
    pub fn raw_fd(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.file.as_raw_fd()
    }
}

impl Drop for VlogFile {
    fn drop(&mut self) {
        if self.delete_on_drop.load(Ordering::Acquire) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Split a verified body into `(key, payload)`.
fn split_body(body: Vec<u8>) -> io::Result<(Vec<u8>, Vec<u8>)> {
    if body.len() < 4 {
        return Err(bad("vlog: body shorter than its key header".into()));
    }
    let key_len = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
    if 4 + key_len > body.len() {
        return Err(bad("vlog: key overruns body".into()));
    }
    let payload = body[4 + key_len..].to_vec();
    let mut key = body;
    key.drain(..4);
    key.truncate(key_len);
    Ok((key, payload))
}

/// Owner callbacks for [`Vlog::compact_below`] — one object, one borrow,
/// so the store can capture its map mutably across both phases.
pub trait CompactOwner {
    /// Is `old` still the owner's live ref for `key`? A record whose ref
    /// was overwritten, deleted, or promoted answers `false` and is
    /// dropped by the compaction.
    fn is_live(&mut self, key: &[u8], old: VlogRef) -> bool;
    /// The record survived and now lives at `new` — swap the cold ref.
    fn moved(&mut self, key: &[u8], old: VlogRef, new: VlogRef);
}

/// Owner-side per-file accounting (bytes are header-inclusive).
struct FileState {
    handle: Arc<VlogFile>,
    bytes: u64,
    live: u64,
}

/// Aggregate gauges for INFO (`vlog_size` / `vlog_dead_bytes` feeders).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VlogStats {
    pub files: usize,
    pub bytes: u64,
    pub live_bytes: u64,
    pub epoch: u64,
}

/// One shard's value log. Single owner (`&mut` appends, the shard
/// thread); concurrent readers go through [`Vlog::pin`].
pub struct Vlog {
    dir: PathBuf,
    rotate_bytes: u64,
    /// Sorted by id; the last entry is the active (append) file.
    files: Vec<FileState>,
    next_id: u32,
    epoch: u64,
    /// In-progress incremental compaction. `Some` = a victim file is
    /// being drained a bounded batch at a time; it stays IN `files`
    /// (readable, pin-safe) until fully drained, so a partly-compacted
    /// file is always consistent: moved records point to their new home,
    /// unmoved ones still read from the victim.
    compaction: Option<CompactCursor>,
}

/// Resumable position within the file currently being compacted.
struct CompactCursor {
    /// Pinned so record reads never borrow `Vlog::files` (freeing the
    /// `&mut self` that `append` needs for the survivor).
    handle: Arc<VlogFile>,
    file_id: u32,
    bytes: u64,
    offset: u64,
}

impl Vlog {
    /// Open a fresh vlog under `dir`, DELETING any leftover vlog files —
    /// the log is per-boot disposable (the AOF is the durability truth).
    pub fn open(dir: &Path, rotate_bytes: u64) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("vlog-") {
                let _ = fs::remove_file(entry.path());
            }
        }
        let mut v = Vlog {
            dir: dir.to_path_buf(),
            rotate_bytes: rotate_bytes.max(1),
            files: Vec::new(),
            next_id: 0,
            epoch: 0,
            compaction: None,
        };
        v.open_next_file()?;
        Ok(v)
    }

    fn open_next_file(&mut self) -> io::Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        let path = self.dir.join(format!("vlog-{id:08}.dat"));
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        self.files.push(FileState {
            handle: Arc::new(VlogFile { id, path, file, delete_on_drop: AtomicBool::new(false) }),
            bytes: 0,
            live: 0,
        });
        Ok(())
    }

    /// Append one record; returns its address. Rotates the active file
    /// past the threshold FIRST, so a record never spans files.
    pub fn append(&mut self, key: &[u8], payload: &[u8]) -> io::Result<VlogRef> {
        let body_len = 4 + key.len() + payload.len();
        if body_len > MAX_BODY as usize {
            return Err(bad(format!("vlog: record too large ({body_len} B)")));
        }
        if self.active().bytes >= self.rotate_bytes {
            self.open_next_file()?;
        }
        let mut body = Vec::with_capacity(body_len);
        body.extend_from_slice(&(key.len() as u32).to_le_bytes());
        body.extend_from_slice(key);
        body.extend_from_slice(payload);
        let mut record = Vec::with_capacity(HEADER as usize + body_len);
        record.extend_from_slice(&(body_len as u32).to_le_bytes());
        record.extend_from_slice(&crc32c(&body).to_le_bytes());
        record.extend_from_slice(&body);

        let state = self.files.last_mut().expect("active file");
        let offset = state.bytes;
        state.handle.file.write_all_at(&record, offset)?;
        state.bytes += record.len() as u64;
        state.live += record.len() as u64;
        Ok(VlogRef { file_id: state.handle.id, offset, len: body_len as u32 })
    }

    fn active(&self) -> &FileState {
        self.files.last().expect("active file")
    }

    fn state_of(&mut self, file_id: u32) -> Option<&mut FileState> {
        self.files.iter_mut().find(|s| s.handle.id == file_id)
    }

    /// Read `(key, payload)` for a ref. For readers that outlive the
    /// owner's borrow (serializer threads), use [`Vlog::pin`] instead.
    pub fn read(&self, r: VlogRef) -> io::Result<(Vec<u8>, Vec<u8>)> {
        match self.files.iter().find(|s| s.handle.id == r.file_id) {
            Some(s) => s.handle.read(r),
            None => Err(bad(format!("vlog: file {} is gone (stale ref?)", r.file_id))),
        }
    }

    /// Pin a file for concurrent / long-lived reading: the returned Arc
    /// keeps the file on disk across compaction until dropped.
    pub fn pin(&self, file_id: u32) -> Option<Arc<VlogFile>> {
        self.files.iter().find(|s| s.handle.id == file_id).map(|s| Arc::clone(&s.handle))
    }

    /// Pin EVERY current file — a point-in-time reader (snapshot view /
    /// AOF rewrite / replication ship) captures this alongside its
    /// frozen refs: any ref frozen at the same instant can only name a
    /// file that exists now, so the whole set keeps the view readable
    /// across compaction for its entire life.
    pub fn pin_all(&self) -> Vec<Arc<VlogFile>> {
        self.files.iter().map(|s| Arc::clone(&s.handle)).collect()
    }

    /// The owner overwrote / deleted / promoted the record at `r`: its
    /// bytes are dead, feeding the compaction trigger.
    pub fn note_dead(&mut self, r: VlogRef) {
        if let Some(s) = self.state_of(r.file_id) {
            s.live = s.live.saturating_sub(HEADER + u64::from(r.len));
        }
    }

    /// The owner dropped its entire cold-ref universe in one stroke
    /// (`FLUSHALL`): every record in every file is now dead. O(files),
    /// no IO — sealed files become full-dead (dropped by the next
    /// [`Self::compact_below`] without a scan); the active file's
    /// garbage bytes fall out at its own retirement.
    pub fn mark_all_dead(&mut self) {
        for s in &mut self.files {
            s.live = 0;
        }
    }

    /// Monotone compaction counter — bumped once per retired file. A
    /// reader holding `(epoch, VlogRef)` can verify in O(1) that no
    /// compaction has run (so its ref cannot have moved).
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn stats(&self) -> VlogStats {
        VlogStats {
            files: self.files.len(),
            bytes: self.files.iter().map(|s| s.bytes).sum(),
            live_bytes: self.files.iter().map(|s| s.live).sum(),
            epoch: self.epoch,
        }
    }

    /// Is there compaction work outstanding at `live_pct`? — a cursor is
    /// mid-file, or some sealed file is below the live ratio. Lets the
    /// caller drive [`Self::compact_step`] on the tick only when there is
    /// something to do (the scan is O(files), no IO).
    pub fn compaction_pending(&self, live_pct: u32) -> bool {
        if self.compaction.is_some() {
            return true;
        }
        let sealed = self.files.len().saturating_sub(1);
        self.files[..sealed]
            .iter()
            .any(|s| s.bytes > 0 && s.live.saturating_mul(100) < s.bytes.saturating_mul(u64::from(live_pct)))
    }

    /// Compact SEALED files whose live ratio fell below `live_pct`
    /// percent (the active file is never compacted), doing at most
    /// `budget` records of work before returning — so a single call
    /// never blocks the caller for a whole-file rewrite. A victim is
    /// drained across successive calls via [`Vlog::compaction`]; it stays
    /// in `files` (readable, pin-safe) until fully drained, then
    /// unlink-on-last-pin + one epoch bump (unchanged retirement
    /// semantics — a ref only becomes invalid when its file is deleted,
    /// which is still atomic per file). Returns records processed this
    /// call (0 = nothing to compact). Call in a loop to drain fully.
    pub fn compact_step(
        &mut self,
        live_pct: u32,
        owner: &mut dyn CompactOwner,
        budget: usize,
    ) -> io::Result<usize> {
        self.compact_step_capped(live_pct, owner, budget, u32::MAX)
    }

    /// Drop fully-dead sealed files (FLUSHALL / full supersession) scan-
    /// free, then arm [`Vlog::compaction`] on the next live-but-below-
    /// threshold victim. Only files with `id < id_below` are eligible.
    /// Returns `true` if a victim was armed, `false` if none remain.
    fn begin_compaction(&mut self, live_pct: u32, id_below: u32) -> bool {
        let sealed = self.files.len().saturating_sub(1);
        let dead: Vec<u32> = self.files[..sealed]
            .iter()
            .filter(|s| s.bytes > 0 && s.live == 0 && s.handle.id < id_below)
            .map(|s| s.handle.id)
            .collect();
        for id in dead {
            if let Some(pos) = self.files.iter().position(|s| s.handle.id == id) {
                let state = self.files.remove(pos);
                state.handle.delete_on_drop.store(true, Ordering::Release);
                self.epoch += 1;
            }
        }
        let sealed = self.files.len().saturating_sub(1);
        let Some(v) = self.files[..sealed].iter().find(|s| {
            s.handle.id < id_below
                && s.live > 0
                && s.live.saturating_mul(100) < s.bytes.saturating_mul(u64::from(live_pct))
        }) else {
            return false;
        };
        self.compaction = Some(CompactCursor {
            handle: Arc::clone(&v.handle),
            file_id: v.handle.id,
            bytes: v.bytes,
            offset: 0,
        });
        true
    }

    /// [`Self::compact_step`] restricted to files with `id < id_below`.
    /// The full-drain wrapper passes a ceiling so files newly SEALED by
    /// this pass's own survivor appends (always higher ids) are never
    /// re-selected — that reproduces the one-shot victim set of the
    /// original compactor and guarantees termination at ANY `live_pct`
    /// (at `live_pct > 100` even a fully-live file qualifies, so without
    /// the ceiling survivors would ping-pong between files forever).
    fn compact_step_capped(
        &mut self,
        live_pct: u32,
        owner: &mut dyn CompactOwner,
        budget: usize,
        id_below: u32,
    ) -> io::Result<usize> {
        if self.compaction.is_none() && !self.begin_compaction(live_pct, id_below) {
            return Ok(0);
        }
        let mut cur = self.compaction.take().expect("set above");
        let mut done = 0usize;
        while cur.offset < cur.bytes && done < budget {
            let (key, payload, body_len) = read_record(&cur.handle, cur.offset)?;
            let old = VlogRef { file_id: cur.file_id, offset: cur.offset, len: body_len };
            if owner.is_live(&key, old) {
                let new = self.append(&key, &payload)?;
                owner.moved(&key, old, new);
            }
            cur.offset += HEADER + u64::from(body_len);
            done += 1;
        }
        if cur.offset >= cur.bytes {
            if let Some(pos) = self.files.iter().position(|s| s.handle.id == cur.file_id) {
                let state = self.files.remove(pos);
                state.handle.delete_on_drop.store(true, Ordering::Release);
            }
            self.epoch += 1;
            // cursor dropped — next call picks the next victim.
        } else {
            self.compaction = Some(cur);
        }
        Ok(done)
    }

    /// Drain compaction fully at `live_pct` (test / single-threaded bulk
    /// paths). Returns retired-file count.
    pub fn compact_below(&mut self, live_pct: u32, owner: &mut dyn CompactOwner) -> io::Result<usize> {
        let epoch0 = self.epoch;
        // Ceiling = every file that exists NOW; survivor appends create
        // higher-id files this pass must not re-select (else it never
        // terminates at live_pct > 100).
        let ceiling = self.next_id;
        while self.compact_step_capped(live_pct, owner, usize::MAX, ceiling)? > 0 {}
        Ok((self.epoch - epoch0) as usize)
    }
}

/// Sequential-scan read of the record at `offset` (compaction path):
/// `(key, payload, body_len)`.
fn read_record(f: &VlogFile, offset: u64) -> io::Result<(Vec<u8>, Vec<u8>, u32)> {
    let mut header = [0u8; HEADER as usize];
    f.file.read_exact_at(&mut header, offset)?;
    let body_len = u32::from_le_bytes(header[..4].try_into().unwrap());
    if body_len > MAX_BODY {
        return Err(bad(format!("vlog: scan hit absurd body_len {body_len}")));
    }
    let (key, payload) = f.read(VlogRef { file_id: f.id, offset, len: body_len })?;
    Ok((key, payload, body_len))
}

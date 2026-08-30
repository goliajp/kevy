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
//!
//! **The payload is a `kevy-compress` frame.** Values from one keyspace
//! land together, so the file is a corpus: each file carries a
//! dictionary trained on a sample of its predecessor's raw payloads
//! (same population, already real bytes — no cold-start window), and
//! records encode against it on append, reaching the cross-value
//! redundancy a per-datum compressor cannot see. The frame lives
//! INSIDE the body, so on-disk framing does not move and the CRC
//! covers exactly the stored bytes. The dictionary lives in the
//! [`VlogFile`] it serves and dies with it — the vlog is disposable,
//! so its dictionaries are too, and no format-compatibility burden
//! ever exists. Incompressible payloads store raw inside the frame
//! (never expanded past the 6-byte frame header).

//! Every public item here is documented, and the lint keeps it that
//! way: kevy-vlog is the value log, and `[workspace.lints.rust] warnings = "deny"`
//! turns a new gap into a compile error rather than a number that
//! drifts. Closed from 65 sites (store) and 7 (vlog) in v6.
#![warn(missing_docs)]
// The checksum comes from the one public front, not a private copy of it.
// `kevy_sys::checksum`'s own docstring names this crate as a consumer and
// says why: "one public front so every consumer (AOF envelope, vlog
// records, immutable segments) speaks the same checksum without re-owning
// the fallback." kevy-seg already did that; this crate carried 73 lines
// saying it could not, because it could not depend on kevy-persist — true,
// and beside the point, since kevy-sys was already an unconditional
// dependency here and this crate never builds for wasm32 (kevy-store gates
// the tier backend off it). Verified byte-identical over 3,075 inputs
// before the copy was removed: it is an on-disk format.
use kevy_sys::checksum::crc32c;

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
    /// Which file in the shard's log holds it. Files are append-only and
    /// never renumbered, so this stays valid until compaction rewrites the
    /// ref — see `epoch`.
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
    /// The corpus model every record in THIS file was encoded against —
    /// trained at rotation from the previous file's raw samples, empty
    /// for the first file. Lives and dies with the file (disposability
    /// is inherited, not engineered).
    dict: Vec<u8>,
}

impl VlogFile {
    /// This file's id, as a `VlogRef` records it.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Read one record back: `(key, payload)`. ONE positional pread of
    /// the whole record image (the body length is already in the ref),
    /// then [`verify_image`] — a length/CRC mismatch is `InvalidData`
    /// (this process wrote the record this boot; a bad read is a bug,
    /// never "corruption to heal").
    pub fn read(&self, r: VlogRef) -> io::Result<(Vec<u8>, Vec<u8>)> {
        let (key, frame) = verify_image(r, self.read_image(r)?)?;
        Ok((key, self.decompress(&frame)?))
    }

    /// Decode a verified record's frame against THIS file's dictionary.
    /// The batched-read path pairs this with [`verify_image`]; a frame
    /// that fails to decode is a process bug by the same doctrine as a
    /// CRC mismatch (this process wrote it this boot).
    pub fn decompress(&self, frame: &[u8]) -> io::Result<Vec<u8>> {
        kevy_compress::decode(&self.dict, frame)
            .map_err(|e| bad(format!("vlog: {e} at file {}", self.id)))
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
    /// Files in the log, including the one currently being appended to.
    pub files: usize,
    /// Bytes on disk across all files — what `vlog_size` reports.
    pub bytes: u64,
    /// Bytes still referenced by a live stub. `bytes - live_bytes` is the
    /// dead fraction compaction exists to reclaim, and is what
    /// `vlog_dead_bytes` reports.
    pub live_bytes: u64,
    /// Compaction generation. A `VlogRef` taken before this changed may
    /// have been moved; see `epoch()`.
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
    /// Raw payload samples from the ACTIVE file — the training corpus
    /// for the NEXT file's dictionary (rotation seeding, RFC §7.2).
    samples: Vec<Vec<u8>>,
    sample_bytes: usize,
}

/// Cap on retained training samples per file. Enough to fill a
/// dictionary several times over; sampling stops once full, so a
/// 256 MiB file never hoards its whole body.
const SAMPLE_BUDGET: usize = 256 << 10;

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
            samples: Vec::new(),
            sample_bytes: 0,
        };
        v.open_next_file(Vec::new())?;
        Ok(v)
    }

    fn open_next_file(&mut self, dict: Vec<u8>) -> io::Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        let path = self.dir.join(format!("vlog-{id:08}.dat"));
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        self.files.push(FileState {
            handle: Arc::new(VlogFile {
                id,
                path,
                file,
                delete_on_drop: AtomicBool::new(false),
                dict,
            }),
            bytes: 0,
            live: 0,
        });
        Ok(())
    }

    /// Rotate: train the new file's dictionary from the outgoing file's
    /// raw samples — same keyspace, same population, already real bytes
    /// (RFC §7.2's rotation seeding; there is no cold-start window in
    /// which records meet an empty model, except the first file's).
    fn rotate(&mut self) -> io::Result<()> {
        let refs: Vec<&[u8]> = self.samples.iter().map(|s| s.as_slice()).collect();
        let dict = kevy_compress::train(&refs, kevy_compress::MAX_OFFSET);
        self.samples.clear();
        self.sample_bytes = 0;
        self.open_next_file(dict)
    }

    /// Append one record; returns its address. Rotates the active file
    /// past the threshold FIRST, so a record never spans files.
    pub fn append(&mut self, key: &[u8], payload: &[u8]) -> io::Result<VlogRef> {
        self.append_level(key, payload, false)
    }

    /// [`Self::append`] with the compaction level: literals
    /// entropy-coded when that wins (strictly smallest-wins, so it can
    /// only shrink). Compaction rewrites through here — a record that
    /// survived to compaction has earned the more expensive encoding
    /// (the RFC's two-stage trade riding a scan that already exists).
    pub(crate) fn append_high(&mut self, key: &[u8], payload: &[u8]) -> io::Result<VlogRef> {
        self.append_level(key, payload, true)
    }

    fn append_level(&mut self, key: &[u8], payload: &[u8], high: bool) -> io::Result<VlogRef> {
        if 4 + key.len() + payload.len() > MAX_BODY as usize {
            return Err(bad(format!(
                "vlog: record too large ({} B)",
                4 + key.len() + payload.len()
            )));
        }
        if self.active().bytes >= self.rotate_bytes {
            self.rotate()?;
        }
        // Raw bytes are the population the NEXT file's dictionary
        // models; sample before encoding.
        if self.sample_bytes < SAMPLE_BUDGET && !payload.is_empty() {
            self.sample_bytes += payload.len();
            self.samples.push(payload.to_vec());
        }
        let dict = &self.active().handle.dict;
        let frame = if high {
            kevy_compress::encode_high(dict, payload)
        } else {
            kevy_compress::encode(dict, payload)
        };
        let body_len = 4 + key.len() + frame.len();
        let mut body = Vec::with_capacity(body_len);
        body.extend_from_slice(&(key.len() as u32).to_le_bytes());
        body.extend_from_slice(key);
        body.extend_from_slice(&frame);
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

    /// A snapshot of the gauges INFO reads. Cheap: the byte totals are
    /// summed over the file list, which is one entry per file rather than
    /// per record.
    pub fn stats(&self) -> VlogStats {
        VlogStats {
            files: self.files.len(),
            bytes: self.files.iter().map(|s| s.bytes).sum(),
            live_bytes: self.files.iter().map(|s| s.live).sum(),
            epoch: self.epoch,
        }
    }

}

mod compact;
pub(crate) use compact::CompactCursor;

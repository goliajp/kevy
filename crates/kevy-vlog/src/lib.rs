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
//! ```
//! use kevy_vlog::Vlog;
//!
//! let dir = kevy_tmpdir::TmpDir::new("vlog-crate-doc");
//! let mut log = Vlog::open(dir.path(), 1 << 20)?;
//! let at = log.append(b"user:1", b"a cold value")?;
//! assert_eq!(log.read(at)?, (b"user:1".to_vec(), b"a cold value".to_vec()));
//! // the owner overwrote or deleted the key: its bytes are now dead weight
//! log.note_dead(at);
//! assert_eq!(log.stats().live_bytes, 0);
//! # Ok::<(), std::io::Error>(())
//! ```
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
// Best-effort removal, on paths where the file is being abandoned.
// A file that will not delete is a stray the next sweep collects,
// and refusing here would abandon the rest of the cleanup.
#![expect(clippy::let_underscore_must_use, reason = "removing what is already meant to be gone")]
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

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Default rotation threshold for the active file (RFC §7: 256 MiB).
///
/// ```
/// let dir = kevy_tmpdir::TmpDir::new("vlog-default-rotate");
/// let v = kevy_vlog::Vlog::open(dir.path(), kevy_vlog::DEFAULT_ROTATE_BYTES)?;
/// assert_eq!(kevy_vlog::DEFAULT_ROTATE_BYTES, 256 * 1024 * 1024);
/// assert_eq!(v.stats().files, 1);
/// # Ok::<(), std::io::Error>(())
/// ```
pub const DEFAULT_ROTATE_BYTES: u64 = 256 << 20;

/// Per-record header: `body_len u32-LE | crc32c u32-LE`.
const HEADER: u64 = 8;

/// Refuse absurd bodies (mirrors the AOF envelope's `MAX_RECORD` bound).
const MAX_BODY: u32 = 1 << 30;

pub(crate) fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Split a verified body into `(key, payload)`.
pub(crate) fn split_body(body: Vec<u8>) -> io::Result<(Vec<u8>, Vec<u8>)> {
    if body.len() < 4 {
        return Err(bad("vlog: body shorter than its key header".into()));
    }
    let key_len =
        u32::from_le_bytes(body[..4].try_into().expect("the body.len() < 4 return above")) as usize;
    if 4 + key_len > body.len() {
        return Err(bad("vlog: key overruns body".into()));
    }
    let payload = body[4 + key_len..].to_vec();
    let mut key = body;
    key.drain(..4);
    key.truncate(key_len);
    Ok((key, payload))
}

mod accounting;
mod record;
mod stats;
pub use accounting::CompressionStats;
pub use record::{CompactOwner, VlogFile, VlogRef, verify_image};
pub use stats::VlogStats;

// Send and Sync are part of the public contract: a change that loses
// either fails to compile here rather than in a caller.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Vlog>();
    send_sync::<VlogFile>();
    send_sync::<VlogRef>();
    send_sync::<VlogStats>();
    send_sync::<CompressionStats>();
};

/// Owner-side per-file accounting (bytes are header-inclusive).
#[derive(Debug)]
struct FileState {
    handle: Arc<VlogFile>,
    bytes: u64,
    live: u64,
    compression: CompressionStats,
}

/// One shard's value log. Single owner (`&mut` appends, the shard
/// thread); concurrent readers go through [`Vlog::pin`].
///
/// ```
/// use kevy_vlog::Vlog;
/// let dir = kevy_tmpdir::TmpDir::new("vlog-type");
/// let mut v = Vlog::open(dir.path(), 1 << 20)?;
/// let r = v.append(b"k", b"cold bytes")?;
/// let file = v.pin(r.file_id).expect("the active file");
/// assert_eq!(file.read(r)?.1, b"cold bytes");
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
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
    /// # Examples
    ///
    /// ```
    /// use kevy_vlog::Vlog;
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-open");
    /// // 1 MiB before the active file rotates.
    /// let v = Vlog::open(dir.path(), 1 << 20).unwrap();
    /// // A fresh log starts with exactly one file and nothing live in it.
    /// assert_eq!(v.stats().files, 1);
    /// assert_eq!(v.stats().live_bytes, 0);
    /// ```
    pub fn open(dir: impl AsRef<Path>, rotate_bytes: u64) -> io::Result<Self> {
        let dir = dir.as_ref();
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
        let compression = CompressionStats { dict_bytes: dict.len() as u64, ..Default::default() };
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
                parsed: kevy_compress::Dict::new(&dict),
            }),
            bytes: 0,
            live: 0,
            compression,
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
    /// # Examples
    ///
    /// ```
    /// use kevy_vlog::Vlog;
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-append");
    /// let mut v = Vlog::open(dir.path(), 1 << 20).unwrap();
    ///
    /// let r = v.append(b"user:1", b"the cold value").unwrap();
    /// // The ref is the address a cold stub keeps; reading it back gives
    /// // the key alongside the payload, so a stub can be re-checked.
    /// assert_eq!(v.read(r).unwrap(), (b"user:1".to_vec(), b"the cold value".to_vec()));
    /// ```
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
        // The file's dictionary, parsed and seeded once at rotation.
        // Taking it per call re-hashed all 65,532 of its positions for
        // every record, which made encode time flat in input size — an
        // 8-byte value cost more than a 6 KiB one.
        let dict = &self.active().handle.parsed;
        let frame = if high { dict.encode_high(payload) } else { dict.encode(payload) };
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
        state.compression.add_record(payload.len(), frame.len());
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
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-read");
    /// let mut v = kevy_vlog::Vlog::open(dir.path(), 1 << 20)?;
    /// let r = v.append(b"k", b"v")?;
    /// assert_eq!(v.read(r)?, (b"k".to_vec(), b"v".to_vec()));
    /// let stale = kevy_vlog::VlogRef::new(99, r.offset, r.len);
    /// assert!(v.read(stale).is_err(), "a file this log never had");
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn read(&self, r: VlogRef) -> io::Result<(Vec<u8>, Vec<u8>)> {
        match self.files.iter().find(|s| s.handle.id == r.file_id) {
            Some(s) => s.handle.read(r),
            None => Err(bad(format!("vlog: file {} is gone (stale ref?)", r.file_id))),
        }
    }

    /// Pin a file for concurrent / long-lived reading: the returned Arc
    /// keeps the file on disk across compaction until dropped.
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-pin-one");
    /// let mut v = kevy_vlog::Vlog::open(dir.path(), 1 << 20)?;
    /// let r = v.append(b"k", b"v")?;
    /// let file = v.pin(r.file_id).expect("the file r names");
    /// assert_eq!(file.id(), r.file_id);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn pin(&self, file_id: u32) -> Option<Arc<VlogFile>> {
        self.files.iter().find(|s| s.handle.id == file_id).map(|s| Arc::clone(&s.handle))
    }

    /// Pin EVERY current file — a point-in-time reader (snapshot view /
    /// AOF rewrite / replication ship) captures this alongside its
    /// frozen refs: any ref frozen at the same instant can only name a
    /// file that exists now, so the whole set keeps the view readable
    /// across compaction for its entire life.
    /// # Examples
    ///
    /// ```
    /// use kevy_vlog::Vlog;
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-pin");
    /// let mut v = Vlog::open(dir.path(), 1 << 20).unwrap();
    /// v.append(b"k", b"v").unwrap();
    ///
    /// // Pinned files survive compaction for as long as the handle lives —
    /// // this is how a snapshot reads a cold value without blocking the
    /// // shard that owns the log.
    /// let pinned = v.pin_all();
    /// assert_eq!(pinned.len(), v.stats().files);
    /// ```
    pub fn pin_all(&self) -> Vec<Arc<VlogFile>> {
        self.files.iter().map(|s| Arc::clone(&s.handle)).collect()
    }

    /// The owner overwrote / deleted / promoted the record at `r`: its
    /// bytes are dead, feeding the compaction trigger.
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-note-dead");
    /// let mut v = kevy_vlog::Vlog::open(dir.path(), 1 << 20)?;
    /// let r = v.append(b"k", b"v")?;
    /// v.note_dead(r);
    /// assert_eq!(v.stats().live_bytes, 0);
    /// // accounting only: the bytes stay readable until compaction
    /// assert_eq!(v.read(r)?.1, b"v");
    /// # Ok::<(), std::io::Error>(())
    /// ```
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
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-mark-all-dead");
    /// let mut v = kevy_vlog::Vlog::open(dir.path(), 1)?;
    /// v.append(b"a", b"1")?;
    /// v.append(b"b", b"2")?;
    /// v.mark_all_dead();
    /// assert_eq!(v.stats().live_bytes, 0);
    /// assert!(v.compaction_pending(50), "the sealed file is now all dead");
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn mark_all_dead(&mut self) {
        for s in &mut self.files {
            s.live = 0;
        }
    }

    /// Monotone compaction counter — bumped once per retired file. A
    /// reader holding `(epoch, VlogRef)` can verify in O(1) that no
    /// compaction has run (so its ref cannot have moved).
    /// # Examples
    ///
    /// ```
    /// use kevy_vlog::Vlog;
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-epoch");
    /// let v = Vlog::open(dir.path(), 1 << 20).unwrap();
    /// // A ref taken while the epoch reads N stays valid until it changes.
    /// assert_eq!(v.epoch(), v.stats().epoch);
    /// ```
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// A snapshot of the gauges INFO reads. Cheap: the byte totals are
    /// summed over the file list, which is one entry per file rather than
    /// per record.
    /// # Examples
    ///
    /// ```
    /// use kevy_vlog::Vlog;
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-stats");
    /// let mut v = Vlog::open(dir.path(), 1 << 20).unwrap();
    /// let r = v.append(b"k", b"payload").unwrap();
    ///
    /// let before = v.stats();
    /// assert!(before.live_bytes > 0);
    ///
    /// // Telling the log a stub is gone moves bytes from live to dead;
    /// // nothing is reclaimed until compaction runs.
    /// v.note_dead(r);
    /// let after = v.stats();
    /// assert_eq!(after.live_bytes, 0);
    /// assert_eq!(after.bytes, before.bytes);
    /// ```
    pub fn stats(&self) -> VlogStats {
        VlogStats {
            files: self.files.len(),
            bytes: self.files.iter().map(|s| s.bytes).sum(),
            live_bytes: self.files.iter().map(|s| s.live).sum(),
            epoch: self.epoch,
        }
    }

    /// Where the value bytes went, over the files still on disk: see
    /// [`CompressionStats`].
    /// # Examples
    ///
    /// ```
    /// use kevy_vlog::Vlog;
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-compression-fn");
    /// let mut v = Vlog::open(dir.path(), 1 << 20).unwrap();
    /// assert_eq!(v.compression().raw_bytes, 0);
    /// v.append(b"k", b"value").unwrap();
    /// // a 5-byte value: one tag byte and one length byte of frame header
    /// assert_eq!(v.compression().raw_bytes, 5);
    /// assert_eq!(v.compression().frame_header_bytes, 2);
    /// ```
    pub fn compression(&self) -> CompressionStats {
        self.files.iter().fold(CompressionStats::default(), |a, s| a.sum(s.compression))
    }
}

mod compact;
pub(crate) use compact::CompactCursor;

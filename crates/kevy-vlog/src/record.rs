//! One record: its address, its file, and the checks that read it back.
//!
//! A record on disk is `HEADER` bytes then a body of `key_len | key | payload`.
//! [`VlogRef`] is the address a cold stub keeps; [`VlogFile`] is one open
//! append-only file; [`verify_image`] is the batched path — hand it the bytes
//! a caller already fetched and it does the same CRC check a single `read`
//! would, without a second syscall.
//!
//! A failed CRC here is a bug in this process, not corruption to repair: the
//! vlog is disposable and the AOF is the durable truth, so the answer is an
//! error, never a heal.

// Best-effort removal, on paths where the file is being abandoned.
// A file that will not delete is a stray the next sweep collects,
// and refusing here would abandon the rest of the cleanup.
#![expect(clippy::let_underscore_must_use, reason = "removing what is already meant to be gone")]

use super::{HEADER, MAX_BODY, bad, crc32c, split_body};
use kevy_sys as _;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
/// The address of one spilled record — what a cold stub holds.
///
/// [`Vlog::append`](crate::Vlog::append) hands one out; an owner that
/// stores the three numbers in its own layout rebuilds it with
/// [`VlogRef::new`].
///
/// ```
/// let dir = kevy_tmpdir::TmpDir::new("vlog-ref-type");
/// let mut v = kevy_vlog::Vlog::open(dir.path(), 1 << 20)?;
/// let first = v.append(b"a", b"1")?;
/// let second = v.append(b"b", b"2")?;
/// // records sit back to back in the active file
/// assert_eq!(second.offset, first.offset + first.disk_len() as u64);
/// assert_eq!(v.read(second)?.1, b"2");
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub struct VlogRef {
    /// Which file in the shard's log holds it. Files are append-only and
    /// never renumbered, so this stays valid until compaction rewrites the
    /// ref — see `epoch`.
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-ref-file-id");
    /// // a 1-byte threshold rotates before every record but the first
    /// let mut v = kevy_vlog::Vlog::open(dir.path(), 1)?;
    /// let a = v.append(b"a", b"1")?;
    /// let b = v.append(b"b", b"2")?;
    /// assert_eq!((a.file_id, b.file_id), (0, 1));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub file_id: u32,
    /// Byte offset of the record HEADER within the file.
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-ref-offset");
    /// let mut v = kevy_vlog::Vlog::open(dir.path(), 1 << 20)?;
    /// let r = v.append(b"k", b"v")?;
    /// assert_eq!(r.offset, 0, "the first record starts the file");
    /// let file = v.pin(r.file_id).expect("the active file");
    /// assert_eq!(file.read_image(r)?.len(), r.disk_len());
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub offset: u64,
    /// Body length (key_len field + key + payload), excluding the header.
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-ref-len");
    /// let mut v = kevy_vlog::Vlog::open(dir.path(), 1 << 20)?;
    /// let r = v.append(b"key", b"")?;
    /// // 4-byte key length, the 3-byte key, and an empty value's
    /// // 2-byte frame header
    /// assert_eq!(r.len, 4 + 3 + 2);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub len: u32,
}

impl VlogRef {
    /// The ref for the record whose header starts at `offset` in file
    /// `file_id`, with a body of `len` bytes — the three numbers a ref
    /// from [`Vlog::append`](crate::Vlog::append) carries.
    ///
    /// ```
    /// use kevy_vlog::{Vlog, VlogRef};
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-ref-new");
    /// let mut v = Vlog::open(dir.path(), 1 << 20)?;
    /// let r = v.append(b"k", b"v")?;
    /// let rebuilt = VlogRef::new(r.file_id, r.offset, r.len);
    /// assert_eq!(v.read(rebuilt)?, (b"k".to_vec(), b"v".to_vec()));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    #[must_use]
    pub const fn new(file_id: u32, offset: u64, len: u32) -> Self {
        VlogRef { file_id, offset, len }
    }

    /// Total on-disk record length: header + body. The image size a
    /// batched reader must fetch at `offset` (see [`verify_image`]).
    #[inline]
    /// # Examples
    ///
    /// ```
    /// use kevy_vlog::Vlog;
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-disklen");
    /// let mut v = Vlog::open(dir.path(), 1 << 20).unwrap();
    /// let r = v.append(b"k", b"v").unwrap();
    /// // Header plus body — the exact byte count a batched reader fetches
    /// // at `r.offset`.
    /// assert!(r.disk_len() > r.len as usize);
    /// ```
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
///
/// Returns the key and the still-encoded frame; decode it with
/// [`VlogFile::decompress`].
///
/// ```
/// use kevy_vlog::verify_image;
/// let dir = kevy_tmpdir::TmpDir::new("vlog-verify-image");
/// let mut v = kevy_vlog::Vlog::open(dir.path(), 1 << 20)?;
/// let r = v.append(b"k", b"value")?;
/// let file = v.pin(r.file_id).expect("the active file");
///
/// let mut image = file.read_image(r)?;
/// let (key, frame) = verify_image(r, image.clone())?;
/// assert_eq!(key, b"k");
/// assert_eq!(file.decompress(&frame)?, b"value");
///
/// // one flipped bit fails the CRC
/// let last = image.len() - 1;
/// image[last] ^= 1;
/// assert!(verify_image(r, image).is_err());
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn verify_image(r: VlogRef, mut image: Vec<u8>) -> io::Result<(Vec<u8>, Vec<u8>)> {
    if image.len() != r.disk_len() {
        return Err(bad(format!(
            "vlog: image length mismatch (want {}, got {})",
            r.disk_len(),
            image.len()
        )));
    }
    let body_len =
        u32::from_le_bytes(image[..4].try_into().expect("the disk_len check returned above"));
    let crc =
        u32::from_le_bytes(image[4..8].try_into().expect("the disk_len check returned above"));
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
///
/// ```
/// use kevy_vlog::{CompactOwner, Vlog, VlogRef};
/// struct NothingLive;
/// impl CompactOwner for NothingLive {
///     fn is_live(&mut self, _: &[u8], _: VlogRef) -> bool {
///         false
///     }
///     fn moved(&mut self, _: &[u8], _: VlogRef, _: VlogRef) {}
/// }
/// let dir = kevy_tmpdir::TmpDir::new("vlog-file-pin");
/// // one record per file
/// let mut v = Vlog::open(dir.path(), 1)?;
/// let r = v.append(b"a", &[7; 64])?;
/// v.append(b"b", b"2")?;
/// let pinned = v.pin(r.file_id).expect("file 0");
/// v.note_dead(r);
/// v.compact_below(100, &mut NothingLive)?;
/// // retired by the log, still readable through the pin
/// assert!(v.read(r).is_err());
/// assert_eq!(pinned.read(r)?.1, [7; 64]);
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
pub struct VlogFile {
    pub(crate) id: u32,
    pub(crate) path: PathBuf,
    pub(crate) file: File,
    pub(crate) delete_on_drop: AtomicBool,
    /// The corpus model every record in THIS file was encoded against —
    /// trained at rotation from the previous file's raw samples, empty
    /// for the first file. Lives and dies with the file (disposability
    /// is inherited, not engineered).
    ///
    /// Parsed and seeded once, here, rather than taken per call. Every
    /// record used to pay to unpack 128 header bytes, Kraft-validate 256
    /// code lengths, rebuild an 8 KiB Huffman decode table, and re-hash
    /// all 65,532 dictionary positions — all pure functions of these
    /// bytes. Measured per value: decode 0.046 -> 1.295 GB/s through
    /// compaction, encode 35.2 -> 0.5 us on the write path.
    pub(crate) parsed: kevy_compress::Dict,
}

impl VlogFile {
    /// This file's id, as a `VlogRef` records it.
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-file-id");
    /// let mut v = kevy_vlog::Vlog::open(dir.path(), 1 << 20)?;
    /// let r = v.append(b"k", b"v")?;
    /// assert_eq!(v.pin_all()[0].id(), r.file_id);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Read one record back: `(key, payload)`. ONE positional pread of
    /// the whole record image (the body length is already in the ref),
    /// then [`verify_image`] — a length/CRC mismatch is `InvalidData`
    /// (this process wrote the record this boot; a bad read is a bug,
    /// never "corruption to heal").
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-file-read");
    /// let mut v = kevy_vlog::Vlog::open(dir.path(), 1 << 20)?;
    /// let r = v.append(b"k", b"cold")?;
    /// let file = v.pin(r.file_id).expect("the active file");
    /// // another thread can take `file` and read without the log
    /// let got = std::thread::spawn(move || file.read(r)).join().expect("reader")?;
    /// assert_eq!(got, (b"k".to_vec(), b"cold".to_vec()));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn read(&self, r: VlogRef) -> io::Result<(Vec<u8>, Vec<u8>)> {
        let (key, frame) = verify_image(r, self.read_image(r)?)?;
        Ok((key, self.decompress(&frame)?))
    }

    /// Decode a **verified** record's frame against THIS file's dictionary.
    ///
    /// The caller must have run [`verify_image`] on the record first, and
    /// this is load-bearing rather than tidy: `kevy-compress` frames carry
    /// no checksum of their own, and a flipped bit that leaves every offset
    /// and length in range decodes to a different value of the right length
    /// — measured at 39-48% of single-bit flips. The CRC checked in
    /// `verify_image` is the only thing standing between a corrupt record
    /// and a plausible wrong answer. `kevy-compress`'s `decode.rs` header
    /// states the same division from the other side.
    ///
    /// Two callers exist and both verify first: `read` just above, and
    /// `kevy_store::tier_serve`'s batched cold read. Nothing in the type
    /// system enforces the pairing — a `VerifiedFrame` newtype would, and
    /// is a v7 item because changing this signature is a major bump.
    ///
    /// A frame that fails to decode is a process bug by the same doctrine
    /// as a CRC mismatch (this process wrote it this boot).
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-decompress");
    /// let mut v = kevy_vlog::Vlog::open(dir.path(), 1 << 20)?;
    /// let r = v.append(b"k", &[b'x'; 300])?;
    /// let file = v.pin(r.file_id).expect("the active file");
    /// let (_, frame) = kevy_vlog::verify_image(r, file.read_image(r)?)?;
    /// assert!(frame.len() < 300, "stored encoded");
    /// assert_eq!(file.decompress(&frame)?, [b'x'; 300]);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn decompress(&self, frame: &[u8]) -> io::Result<Vec<u8>> {
        self.parsed.decode(frame).map_err(|e| bad(format!("vlog: {e} at file {}", self.id)))
    }

    /// Fetch the raw record image (`r.disk_len()` bytes at `r.offset`)
    /// in one pread, UNverified — the batched-read issuance half; pair
    /// with [`verify_image`] on completion.
    ///
    /// ```
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-read-image");
    /// let mut v = kevy_vlog::Vlog::open(dir.path(), 1 << 20)?;
    /// let r = v.append(b"k", b"v")?;
    /// let image = v.pin(r.file_id).expect("the active file").read_image(r)?;
    /// // the header's first four bytes are the body length the ref carries
    /// assert_eq!(image.len(), r.disk_len());
    /// assert_eq!(image[..4], r.len.to_le_bytes());
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn read_image(&self, r: VlogRef) -> io::Result<Vec<u8>> {
        let mut image = vec![0u8; r.disk_len()];
        self.file.read_exact_at(&mut image, r.offset)?;
        Ok(image)
    }
}

/// The underlying file descriptor — what an io_uring batch reader preps
/// its READ SQEs against. The fd stays valid for the life of this pin
/// (the whole point of holding the `Arc<VlogFile>`).
///
/// ```
/// use std::os::fd::AsRawFd;
/// let dir = kevy_tmpdir::TmpDir::new("vlog-fd");
/// let mut v = kevy_vlog::Vlog::open(dir.path(), 1 << 20)?;
/// let r = v.append(b"k", b"v")?;
/// assert!(v.pin(r.file_id).expect("the active file").as_raw_fd() >= 0);
/// # Ok::<(), std::io::Error>(())
/// ```
impl AsRawFd for VlogFile {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.file.as_raw_fd()
    }
}

/// The descriptor, borrowed for as long as the file is.
impl AsFd for VlogFile {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl Drop for VlogFile {
    fn drop(&mut self) {
        if self.delete_on_drop.load(Ordering::Acquire) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Owner callbacks for [`Vlog::compact_step`](crate::Vlog::compact_step)
/// and [`Vlog::compact_below`](crate::Vlog::compact_below) — one object,
/// one borrow, so the store can capture its map mutably across both
/// phases.
///
/// Implemented by whoever holds the refs (the keyspace), so the trait is
/// open. An implementation must uphold:
///
/// - `is_live` answers from the owner's current refs: `true` exactly
///   when the owner still reaches the record at `old`. A record answered
///   `false` is not copied and is gone once its file retires.
/// - `moved` is called only right after `is_live` answered `true` for the
///   same `(key, old)`, and must repoint the owner from `old` to `new`
///   before returning: when the victim file finishes draining it is
///   deleted, and a ref still aimed at it reads a vanished file.
/// - `key` is the key the record was appended with; an owner that has
///   since renamed the key resolves it itself.
///
/// ```
/// use kevy_vlog::{CompactOwner, Vlog, VlogRef};
/// struct One(VlogRef);
/// impl CompactOwner for One {
///     fn is_live(&mut self, _key: &[u8], old: VlogRef) -> bool {
///         old == self.0
///     }
///     fn moved(&mut self, _key: &[u8], _old: VlogRef, new: VlogRef) {
///         self.0 = new;
///     }
/// }
/// let dir = kevy_tmpdir::TmpDir::new("vlog-owner-doc");
/// let mut v = Vlog::open(dir.path(), 64)?;
/// let dead = v.append(b"a", &[0; 64])?;
/// let mut owner = One(v.append(b"b", &[1; 64])?);
/// v.append(b"c", b"rotate")?;
/// v.note_dead(dead);
/// v.compact_below(100, &mut owner)?;
/// assert_eq!(v.read(owner.0)?.0, b"b");
/// # Ok::<(), std::io::Error>(())
/// ```
pub trait CompactOwner {
    /// Is `old` still the owner's live ref for `key`? A record whose ref
    /// was overwritten, deleted, or promoted answers `false` and is
    /// dropped by the compaction.
    ///
    /// ```
    /// use kevy_vlog::{CompactOwner, Vlog, VlogRef};
    /// // an owner that keeps only the key "keep"
    /// struct KeepOne(Option<VlogRef>);
    /// impl CompactOwner for KeepOne {
    ///     fn is_live(&mut self, key: &[u8], _old: VlogRef) -> bool {
    ///         key == b"keep"
    ///     }
    ///     fn moved(&mut self, _key: &[u8], _old: VlogRef, new: VlogRef) {
    ///         self.0 = Some(new);
    ///     }
    /// }
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-is-live");
    /// // two records fill a 40-byte file; the third rotates
    /// let mut v = Vlog::open(dir.path(), 40)?;
    /// let drop_me = v.append(b"drop", b"0123456789abcdef")?;
    /// v.append(b"keep", b"fedcba9876543210")?;
    /// v.append(b"next", b"rotated")?;
    /// v.note_dead(drop_me);
    /// let mut owner = KeepOne(None);
    /// v.compact_below(100, &mut owner)?;
    /// let kept = owner.0.expect("the live record moved");
    /// assert_eq!(v.read(kept)?.0, b"keep");
    /// # Ok::<(), std::io::Error>(())
    /// ```
    fn is_live(&mut self, key: &[u8], old: VlogRef) -> bool;
    /// The record survived and now lives at `new` — swap the cold ref.
    ///
    /// ```
    /// use kevy_vlog::{CompactOwner, Vlog, VlogRef};
    /// struct Log(Vec<(VlogRef, VlogRef)>);
    /// impl CompactOwner for Log {
    ///     fn is_live(&mut self, _: &[u8], _: VlogRef) -> bool {
    ///         true
    ///     }
    ///     fn moved(&mut self, _: &[u8], old: VlogRef, new: VlogRef) {
    ///         self.0.push((old, new));
    ///     }
    /// }
    /// let dir = kevy_tmpdir::TmpDir::new("vlog-moved");
    /// // one record per file
    /// let mut v = Vlog::open(dir.path(), 1)?;
    /// let r = v.append(b"a", &[3; 64])?;
    /// v.append(b"b", b"rotated")?;
    /// let mut log = Log(Vec::new());
    /// // a live ratio above 100% selects every sealed file
    /// v.compact_below(101, &mut log)?;
    /// let (old, new) = log.0[0];
    /// assert_eq!(old, r);
    /// assert_ne!(new.file_id, r.file_id);
    /// assert_eq!(v.read(new)?.1, [3; 64]);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    fn moved(&mut self, key: &[u8], old: VlogRef, new: VlogRef);
}

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
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub fn decompress(&self, frame: &[u8]) -> io::Result<Vec<u8>> {
        kevy_compress::decode_with(&self.parsed, frame)
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

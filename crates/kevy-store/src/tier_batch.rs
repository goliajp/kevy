//! The cold-batch read types behind [`Store::peek_hash_rows`]. Split
//! from `tier_serve.rs` for the 500-LOC house rule.

use std::sync::Arc;

use kevy_vlog::{VlogFile, VlogRef};

#[cfg(doc)]
use crate::Store;
use crate::StoreError;

/// One planned cold-record read in a [`Store::peek_hash_rows`]
/// batch. The pinned file keeps the record readable even if a
/// compaction retires the file mid-batch.
///
/// ```
/// use kevy_store::{ColdBatchReader, ColdRead, Store, SyncColdRead};
///
/// // a reader that logs each planned read, then reads it the default way
/// struct Logged(Vec<ColdRead>);
/// impl ColdBatchReader for Logged {
///     fn read_batch(&mut self, reads: &[ColdRead]) -> std::io::Result<(Vec<Vec<u8>>, u64)> {
///         self.0.extend_from_slice(reads);
///         SyncColdRead.read_batch(reads)
///     }
/// }
/// # let dir = std::env::temp_dir().join(format!("kevy-doc-coldread-{}", std::process::id()));
/// let mut s = Store::new();
/// s.enable_tiering(&dir, 1 << 20)?;
/// s.hset(b"row", &[(b"f".as_slice(), [b'x'; 4096].as_slice())])?;
/// s.set_tier_budget(1);
/// s.demote_to_watermark();
/// let mut log = Logged(Vec::new());
/// s.peek_hash_rows(&[b"row".as_slice()], &[b"f".as_slice()], &mut log);
/// assert_eq!(log.0.len(), 1, "one cold row, one planned read");
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ColdRead {
    /// Pinned vlog file the record lives in.
    ///
    /// ```
    /// use kevy_store::{ColdBatchReader, ColdRead, Store, SyncColdRead};
    /// struct Files(Vec<u32>);
    /// impl ColdBatchReader for Files {
    ///     fn read_batch(&mut self, reads: &[ColdRead]) -> std::io::Result<(Vec<Vec<u8>>, u64)> {
    ///         self.0.extend(reads.iter().map(|r| r.file.id()));
    ///         SyncColdRead.read_batch(reads)
    ///     }
    /// }
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-coldfile-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.hset(b"row", &[(b"f".as_slice(), [b'x'; 4096].as_slice())])?;
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// let mut files = Files(Vec::new());
    /// s.peek_hash_rows(&[b"row".as_slice()], &[b"f".as_slice()], &mut files);
    /// // the record sits in the tier's first (and only) file
    /// assert_eq!(files.0, [s.tier_pins()[0].id()]);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub file: Arc<VlogFile>,
    /// Record address; the image to fetch is `vref.disk_len()`
    /// bytes at `vref.offset`.
    ///
    /// ```
    /// use kevy_store::{ColdBatchReader, ColdRead, Store, SyncColdRead};
    /// struct Sizes(Vec<(u64, usize)>);
    /// impl ColdBatchReader for Sizes {
    ///     fn read_batch(&mut self, reads: &[ColdRead]) -> std::io::Result<(Vec<Vec<u8>>, u64)> {
    ///         self.0.extend(reads.iter().map(|r| (r.vref.offset, r.vref.disk_len())));
    ///         let (images, n) = SyncColdRead.read_batch(reads)?;
    ///         assert!(images.iter().zip(&self.0).all(|(img, (_, len))| img.len() == *len));
    ///         Ok((images, n))
    ///     }
    /// }
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-coldvref-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.hset(b"row", &[(b"f".as_slice(), [b'x'; 4096].as_slice())])?;
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// let mut sizes = Sizes(Vec::new());
    /// s.peek_hash_rows(&[b"row".as_slice()], &[b"f".as_slice()], &mut sizes);
    /// assert_eq!(sizes.0[0].0, 0, "the first record starts the file");
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub vref: VlogRef,
}

/// The read-issuance half of a cold batch: fetch every record
/// image, in `reads` order. [`SyncColdRead`] is the ordered
/// positional-read loop (poller reactors + embedded); the server's
/// io_uring backend submits the batch to a secondary ring instead.
///
/// Open for implementation, so a host can issue the reads its own
/// way. An implementation must return exactly one image per read, in
/// `reads` order, each the `vref.disk_len()` bytes at `vref.offset` of
/// `file` (the store verifies and decodes them, so a wrong byte is
/// caught, but a missing or reordered image pairs a record with the
/// wrong key); an `Err` fails the whole batch, and the store treats
/// none of it as read.
///
/// ```
/// use kevy_store::{ColdBatchReader, ColdRead, Store};
///
/// // issue the reads in reverse, hand the images back in order
/// struct Reversed;
/// impl ColdBatchReader for Reversed {
///     fn read_batch(&mut self, reads: &[ColdRead]) -> std::io::Result<(Vec<Vec<u8>>, u64)> {
///         let mut images = vec![Vec::new(); reads.len()];
///         for (i, r) in reads.iter().enumerate().rev() {
///             images[i] = r.file.read_image(r.vref)?;
///         }
///         Ok((images, 1))
///     }
/// }
/// # let dir = std::env::temp_dir().join(format!("kevy-doc-batchreader-{}", std::process::id()));
/// let mut s = Store::new();
/// s.enable_tiering(&dir, 1 << 20)?;
/// s.hset(b"a", &[(b"f".as_slice(), [b'1'; 4096].as_slice())])?;
/// s.hset(b"b", &[(b"f".as_slice(), [b'2'; 4096].as_slice())])?;
/// s.set_tier_budget(1);
/// s.demote_to_watermark();
/// let rows = s.peek_hash_rows(&[b"a".as_slice(), b"b"], &[b"f".as_slice()], &mut Reversed);
/// assert_eq!(rows[1].as_ref().unwrap().as_ref().unwrap()[0].as_deref(), Some(&[b'2'; 4096][..]));
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub trait ColdBatchReader {
    /// Fetch each `reads[i]`'s raw image (`vref.disk_len()` bytes
    /// at `vref.offset`, unverified — the store runs
    /// [`verify_image`](kevy_vlog::verify_image) + decode on completion). Returns the images
    /// plus the number of kernel submissions made (1 for the sync
    /// loop, ceil(n / ring entries) for a ring).
    ///
    /// ```
    /// use kevy_store::{ColdBatchReader, ColdRead, Store, SyncColdRead};
    /// // report one submission per read instead of one per batch
    /// struct OneEach;
    /// impl ColdBatchReader for OneEach {
    ///     fn read_batch(&mut self, reads: &[ColdRead]) -> std::io::Result<(Vec<Vec<u8>>, u64)> {
    ///         let (images, _) = SyncColdRead.read_batch(reads)?;
    ///         Ok((images, reads.len() as u64))
    ///     }
    /// }
    /// # let dir = std::env::temp_dir().join(format!("kevy-doc-readbatch-{}", std::process::id()));
    /// let mut s = Store::new();
    /// s.enable_tiering(&dir, 1 << 20)?;
    /// s.hset(b"a", &[(b"f".as_slice(), [b'1'; 4096].as_slice())])?;
    /// s.hset(b"b", &[(b"f".as_slice(), [b'2'; 4096].as_slice())])?;
    /// s.set_tier_budget(1);
    /// s.demote_to_watermark();
    /// s.peek_hash_rows(&[b"a".as_slice(), b"b"], &[b"f".as_slice()], &mut OneEach);
    /// assert_eq!(s.tier_stats().batch_submissions_total, 2);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    fn read_batch(&mut self, reads: &[ColdRead]) -> std::io::Result<(Vec<Vec<u8>>, u64)>;
}

/// The default reader: one ordered `pread` per record.
///
/// ```
/// use kevy_store::{Store, SyncColdRead};
/// # let dir = std::env::temp_dir().join(format!("kevy-doc-syncread-{}", std::process::id()));
/// let mut s = Store::new();
/// s.enable_tiering(&dir, 1 << 20)?;
/// s.hset(b"row", &[(b"f".as_slice(), b"v".as_slice()), (b"pad", [b'x'; 4096].as_slice())])?;
/// s.set_tier_budget(1);
/// s.demote_to_watermark();
/// let rows = s.peek_hash_rows(&[b"row".as_slice()], &[b"f".as_slice()], &mut SyncColdRead);
/// assert_eq!(rows[0], Ok(Some(vec![Some(b"v".to_vec())])));
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct SyncColdRead;

impl ColdBatchReader for SyncColdRead {
    fn read_batch(&mut self, reads: &[ColdRead]) -> std::io::Result<(Vec<Vec<u8>>, u64)> {
        let mut images = Vec::with_capacity(reads.len());
        for r in reads {
            images.push(r.file.read_image(r.vref)?);
        }
        Ok((images, 1))
    }
}

/// One peeked row: the per-field values of a live hash
/// (`Ok(Some(..))`, one `Option` per requested field), a missing
/// key (`Ok(None)`), or a non-hash (`Err(WrongType)`).
///
/// ```
/// use kevy_store::{PeekRow, SetCondition, Store, StoreError};
/// let mut s = Store::new();
/// s.hset(b"h", &[(b"a".as_slice(), b"1".as_slice())])?;
/// s.set(b"str", b"x".to_vec(), None, SetCondition::Always);
/// let hit: PeekRow = s.peek_hash_fields(b"h", &[b"a".as_slice(), b"zz"]);
/// assert_eq!(hit, Ok(Some(vec![Some(b"1".to_vec()), None])));
/// assert_eq!(s.peek_hash_fields(b"missing", &[b"a".as_slice()]), Ok(None));
/// assert_eq!(s.peek_hash_fields(b"str", &[b"a".as_slice()]), Err(StoreError::WrongType));
/// # Ok::<(), StoreError>(())
/// ```
pub type PeekRow = Result<Option<Vec<Option<Vec<u8>>>>, StoreError>;

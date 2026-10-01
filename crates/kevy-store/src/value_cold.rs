//! The cold-value stub — the 24-byte in-map remainder of a demoted
//! value, shared by both backings (the per-boot vlog and the
//! persistent row segments). Split from `value.rs` for the 500-LOC
//! house rule.

/// Type tag a [`ColdRef`] carries so `TYPE` / SCAN's `TYPE` filter / the
/// WRONGTYPE precheck answer with zero IO.
///
/// ```
/// use kevy_store::{COLD_TAG_HASH, COLD_TAG_STRING, SetCondition, Store, StoreError};
/// # let dir = std::env::temp_dir().join(format!("kevy-doc-tag-string-{}", std::process::id()));
/// assert_ne!(COLD_TAG_STRING, COLD_TAG_HASH);
/// let mut s = Store::new();
/// s.enable_tiering(&dir, 1 << 20)?;
/// s.set(b"k", vec![b'x'; 4096], None, SetCondition::Always);
/// s.set_tier_budget(1);
/// s.demote_to_watermark();
/// // the string tag answers TYPE and refuses a hash read with no disk read
/// assert_eq!(s.type_of(b"k"), "string");
/// assert_eq!(s.hget(b"k", b"f").map(|_| ()), Err(StoreError::WrongType));
/// assert_eq!(s.tier_stats().preads_total, 0);
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub const COLD_TAG_STRING: u8 = 1;
/// Hash tag — see [`COLD_TAG_STRING`].
///
/// ```
/// use kevy_store::{Store, StoreError};
/// # let dir = std::env::temp_dir().join(format!("kevy-doc-tag-hash-{}", std::process::id()));
/// let mut s = Store::new();
/// s.enable_tiering(&dir, 1 << 20)?;
/// s.hset(b"row", &[(b"f".as_slice(), [b'x'; 4096].as_slice())])?;
/// s.set_tier_budget(1);
/// s.demote_to_watermark();
/// assert_eq!(s.type_of(b"row"), "hash");
/// assert_eq!(s.get(b"row").map(|_| ()), Err(StoreError::WrongType));
/// assert_eq!(s.tier_stats().preads_total, 0);
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub const COLD_TAG_HASH: u8 = 2;

/// The in-map stub a demoted (cold) value leaves behind: its vlog
/// record's address + enough metadata to answer stage-1 questions
/// (existence, TYPE, weight) with zero IO. Byte math: offset u64 (8) +
/// file_id/len/weight u32 (12) + type_tag/touched u8 (2) = 22, padded
/// to 24 by u64 alignment — fits `Value`'s 24 B payload (≤32 B assert).
///
/// ```
/// use kevy_store::{ColdRef, SetCondition, Store, Value};
/// # let dir = std::env::temp_dir().join(format!("kevy-doc-coldref-{}", std::process::id()));
/// let mut s = Store::new();
/// s.enable_tiering(&dir, 1 << 20)?;
/// s.set(b"k", vec![b'x'; 4096], None, SetCondition::Always);
/// s.set_tier_budget(1);
/// s.demote_to_watermark();
/// let mut stub = None;
/// s.snapshot_each(|_, v, _| if let Value::Cold(c) = v { stub = Some(*c) });
/// // a vlog-backed stub, not a row-segment one
/// assert_eq!(stub.and_then(ColdRef::seg_parts), None);
/// // a row-segment stub round-trips through its snapshot record
/// assert_eq!(ColdRef::from_seg_parts(7, 512).seg_parts(), Some((7, 512)));
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ColdRef {
    /// Byte offset of the record header inside its vlog file.
    pub(crate) offset: u64,
    /// The vlog file id.
    pub(crate) file_id: u32,
    /// Record body length (mirrors `kevy_vlog::VlogRef::len`).
    pub(crate) len: u32,
    /// The ORIGINAL value weight (heap bytes) at demotion time —
    /// promotion re-accounting sanity + spill-policy input. The live
    /// `Entry::weight` is re-stamped to the stub's actual footprint so
    /// `MEMORY USAGE` (and Σ ≈ used_memory) stay stub-actual.
    pub(crate) weight: u32,
    /// [`COLD_TAG_STRING`] / [`COLD_TAG_HASH`].
    pub(crate) type_tag: u8,
    /// Promotion-gate probation mark: the first materializing access
    /// serves bytes via pread WITHOUT installing and sets this; the
    /// second access promotes. Bulk/shared-lane reads never set it.
    pub(crate) touched: u8,
}

/// High bit of `ColdRef::file_id`: the stub's backing store is a row
/// SEGMENT (persistent, keyed by the row key) rather than the per-boot
/// vlog. Segment stubs reuse the same 24-byte shape — `offset` holds
/// the segment's stable seq, `len` is unused.
pub(crate) const SEG_BACKING: u32 = 1 << 31;

impl ColdRef {
    /// `(seq, value_weight)` when this stub's backing is a row
    /// segment; `None` for a vlog stub. Public for the persistence
    /// layer: the snapshot's stub record carries exactly these.
    pub fn seg_parts(self) -> Option<(u32, u32)> {
        (self.file_id & SEG_BACKING != 0).then_some((self.offset as u32, self.weight))
    }

    /// Rebuild a row-segment stub from its snapshot record.
    pub fn from_seg_parts(seq: u32, value_weight: u32) -> Self {
        ColdRef {
            offset: u64::from(seq),
            file_id: SEG_BACKING,
            len: 0,
            weight: value_weight,
            type_tag: COLD_TAG_HASH,
            touched: 0,
        }
    }

    /// The tag's Redis type name (the `TYPE` command on a cold key —
    /// answered from RAM, never a pread).
    pub(crate) fn type_name(self) -> &'static str {
        match self.type_tag {
            COLD_TAG_HASH => "hash",
            _ => "string",
        }
    }
}

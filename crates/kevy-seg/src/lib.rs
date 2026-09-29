//! Immutable ordered-record segment files — build once, binary-search
//! forever. The storage stone every cold tier shares: row segments,
//! scalar-index cold segments and text bucket segments differ only in
//! what their records MEAN; how records are laid out, located,
//! checksummed and retired is identical, so it lives here once.
//!
//! ```
//! use kevy_seg::{Manifest, ManifestEntry, Seg, SegBuilder};
//!
//! # let dir = std::env::temp_dir().join(format!("kevy-seg-crate-doc-{}", std::process::id()));
//! # std::fs::create_dir_all(&dir)?;
//! // keys go in strictly ascending; the footer seals the file
//! let mut b = SegBuilder::create(&dir.join("s1.seg"))?;
//! b.push(b"a", b"1")?;
//! b.push(b"b", b"2")?;
//! let meta = b.finish()?;
//!
//! // the manifest makes the sealed segment part of the live set
//! Manifest::open(&dir)?.add(ManifestEntry::new("s1.seg", meta))?;
//!
//! let seg = Seg::open(&dir.join("s1.seg"))?;
//! assert_eq!(seg.get(b"b")?, Some(b"2".to_vec()));
//! assert_eq!(seg.count_range(b"a", b"z")?, 2);
//! # std::fs::remove_dir_all(&dir).ok();
//! # Ok::<(), kevy_seg::SegError>(())
//! ```
//!
//! # Model
//!
//! [`SegBuilder`] appends records in strictly ascending key order,
//! pages them, checksums every page, and seals the file with a footer
//! (record count, min/max key, and a fence table — the first key of
//! every page). [`Seg`] opens the sealed file, keeps the fence table
//! in memory, and answers `get` / `range` / `count_range` with one
//! fence binary search plus one page read. Nothing ever mutates a
//! sealed segment; deletion is the caller's directory concern
//! (tombstones live above this crate), and removal is `unlink`.
//!
//! # Layout (v1)
//!
//! Data pages are 4 KiB: a small header, cells packed forward, a slot
//! directory (u16 cell offsets) packed backward from the tail, and a
//! CRC32C over the page in the last 4 bytes. A record too large for
//! one page spills its payload into a run of dedicated overflow pages
//! (the cell keeps the key and points at the run — SQLite's overflow
//! idea, shorn of its freelist because nothing here is ever freed).
//! The footer is written last: fence entries, min/max keys, counts,
//! magic, and its own CRC; the final 16 bytes locate and size it.
//!
//! # References
//!
//! The read-only half of SQLite's page format (slot directory growing
//! backward, overflow chains) is the reference for the data pages.
//! Its WAL, freelist, cursors and varint cell headers are deliberately
//! absent: an immutable segment needs none of them.

#![warn(missing_docs)]

mod builder;
mod layout;
mod manifest;
mod reader;

pub use builder::SegBuilder;
pub use manifest::{Manifest, ManifestEntry};
pub use reader::{RangeIter, Seg};

// Send and Sync are part of the public contract: a change that loses
// either fails to compile here rather than in a caller.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Seg>();
    send_sync::<SegBuilder>();
    send_sync::<SegMeta>();
    send_sync::<Manifest>();
    send_sync::<ManifestEntry>();
    send_sync::<SegError>();
};

/// Sealed-segment summary, from the footer.
///
/// ```
/// # let dir = std::env::temp_dir().join(format!("kevy-segmeta-doc-{}", std::process::id()));
/// # std::fs::create_dir_all(&dir)?;
/// let mut b = kevy_seg::SegBuilder::create(&dir.join("a.seg"))?;
/// b.push(b"k1", b"v")?;
/// b.push(b"k2", b"v")?;
/// let meta = b.finish()?;
/// assert_eq!((meta.records, meta.min_key.as_slice(), meta.max_key.as_slice()), (2, &b"k1"[..], &b"k2"[..]));
/// # std::fs::remove_dir_all(&dir).ok();
/// # Ok::<(), kevy_seg::SegError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct SegMeta {
    /// Records in the segment.
    pub records: u64,
    /// Data pages (excluding overflow and footer pages).
    pub data_pages: u32,
    /// Smallest key.
    pub min_key: Vec<u8>,
    /// Largest key.
    pub max_key: Vec<u8>,
}

/// Why a segment file was refused at open. Corruption is a refusal,
/// never a silent partial read.
#[derive(Debug)]
#[non_exhaustive]
pub enum SegError {
    /// OS-level failure.
    Io(std::io::Error),
    /// Not a segment / truncated / bit-rotted — the named reason.
    Corrupt(&'static str),
    /// Builder misuse: keys not strictly ascending.
    Unsorted,
}

impl std::fmt::Display for SegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Corrupt(why) => write!(f, "corrupt segment: {why}"),
            Self::Unsorted => write!(f, "keys must be strictly ascending"),
        }
    }
}

impl std::error::Error for SegError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SegError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

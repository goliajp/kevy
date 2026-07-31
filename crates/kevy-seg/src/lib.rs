//! Immutable ordered-record segment files — build once, binary-search
//! forever. The storage stone every cold tier shares: row segments,
//! scalar-index cold segments and text bucket segments differ only in
//! what their records MEAN; how records are laid out, located,
//! checksummed and retired is identical, so it lives here once.
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

mod builder;
mod layout;
mod reader;

pub use builder::SegBuilder;
pub use reader::{RangeIter, Seg};

/// Sealed-segment summary, from the footer.
#[derive(Debug, Clone, PartialEq, Eq)]
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

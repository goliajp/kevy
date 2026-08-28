//! Reading a sealed segment: trailer → footer → fence table in memory;
//! every lookup is one fence binary search + one page read (+ overflow
//! pages when the record spilled). Corruption at any layer is a named
//! refusal at open or at read — never a silent partial answer.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::layout::{self, Cell, OVERFLOW_CAP, PAGE, TRAILER};
use crate::{SegError, SegMeta};

/// An open segment. Cheap to clone-by-Arc above this crate; internally
/// one file handle plus the in-memory fence table.
pub struct Seg {
    f: File,
    meta: SegMeta,
    /// (page_index, first_key), ascending by key — the binary-search
    /// directory over data pages.
    fences: Vec<(u32, Vec<u8>)>,
}

impl Seg {
    /// Open and verify the trailer + footer. Data pages verify lazily
    /// on first touch (a cold segment can be huge; open stays O(footer)).
    pub fn open(path: &Path) -> Result<Self, SegError> {
        let f = File::open(path)?;
        let len = f.metadata()?.len();
        if len < TRAILER as u64 {
            return Err(SegError::Corrupt("shorter than the trailer"));
        }
        let mut tr = [0u8; TRAILER];
        f.read_exact_at(&mut tr, len - TRAILER as u64)?;
        if u32::from_le_bytes(tr[12..16].try_into().expect("4")) != layout::MAGIC {
            return Err(SegError::Corrupt("bad magic"));
        }
        let footer_off = u64::from_le_bytes(tr[0..8].try_into().expect("8"));
        let footer_len = u32::from_le_bytes(tr[8..12].try_into().expect("4")) as usize;
        // Checked: both values are attacker-controlled bytes at this
        // point, and a wrapping sum must refuse, not panic.
        let closes = (footer_len as u64)
            .checked_add(TRAILER as u64)
            .and_then(|t| footer_off.checked_add(t))
            == Some(len);
        if !closes {
            return Err(SegError::Corrupt("trailer does not close the file"));
        }
        let mut footer = vec![0u8; footer_len];
        f.read_exact_at(&mut footer, footer_off)?;
        let (records, data_pages, min_key, max_key, fences) =
            layout::decode_footer(&footer).ok_or(SegError::Corrupt("footer crc/shape"))?;
        Ok(Self {
            f,
            meta: SegMeta { records, data_pages, min_key, max_key },
            fences,
        })
    }

    /// Sealed-segment summary.
    pub fn meta(&self) -> &SegMeta {
        &self.meta
    }

    /// The record at `key`, if present.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, SegError> {
        if key < self.meta.min_key.as_slice() || key > self.meta.max_key.as_slice() {
            return Ok(None);
        }
        let page = self.read_page(self.fence_page(key))?;
        let n = layout::page_slots(&page);
        let (mut lo, mut hi) = (0u16, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let cell = layout::read_cell(&page, layout::slot_offset(&page, mid))
                .ok_or(SegError::Corrupt("cell shape"))?;
            match cell.key().cmp(key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return self.materialize(cell).map(Some),
            }
        }
        Ok(None)
    }

    /// Records with `lo <= key <= hi`, ascending.
    pub fn range(&self, lo: &[u8], hi: &[u8]) -> RangeIter<'_> {
        RangeIter {
            seg: self,
            page_ix: self.fence_page(lo),
            slot: 0,
            lo: lo.to_vec(),
            hi: hi.to_vec(),
            page: None,
            done: false,
        }
    }

    /// How many records fall in `lo..=hi` — two fence descents plus at
    /// most two page walks; the pages between are counted whole. The
    /// cross-window COUNT primitive.
    pub fn count_range(&self, lo: &[u8], hi: &[u8]) -> Result<u64, SegError> {
        if lo > hi || self.fences.is_empty() {
            return Ok(0);
        }
        let (first, last) = (self.fence_page(lo), self.fence_page(hi));
        let mut n = 0u64;
        for ix in first..=last {
            let page = self.read_page(ix)?;
            let slots = layout::page_slots(&page);
            if ix != first && ix != last {
                n += u64::from(slots);
                continue;
            }
            for s in 0..slots {
                let cell = layout::read_cell(&page, layout::slot_offset(&page, s))
                    .ok_or(SegError::Corrupt("cell shape"))?;
                if cell.key() >= lo && cell.key() <= hi {
                    n += 1;
                }
            }
        }
        // Whole-page middles assume interior pages lie inside [lo,hi]:
        // true because fences ascend and first/last bound the range.
        Ok(n)
    }

    /// Index into `fences` of the page that may hold `key` — the last
    /// fence with first_key <= key.
    fn fence_page(&self, key: &[u8]) -> usize {
        match self.fences.binary_search_by(|(_, k)| k.as_slice().cmp(key)) {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        }
    }

    /// Read + verify data page `fences[ix]`.
    fn read_page(&self, ix: usize) -> Result<Vec<u8>, SegError> {
        let page_index = self.fences[ix].0;
        let mut buf = vec![0u8; PAGE];
        self.f.read_exact_at(&mut buf, u64::from(page_index) * PAGE as u64)?;
        if !layout::page_intact(&buf) {
            return Err(SegError::Corrupt("data page crc"));
        }
        Ok(buf)
    }

    /// Inline payloads copy out; overflow payloads gather their run.
    fn materialize(&self, cell: Cell<'_>) -> Result<Vec<u8>, SegError> {
        match cell {
            Cell::Inline { payload, .. } => Ok(payload.to_vec()),
            Cell::Overflow { total_len, first_page, n_pages, .. } => {
                let mut out =
                    Vec::with_capacity(layout::capped_capacity(total_len as usize));
                for p in 0..n_pages {
                    let mut buf = vec![0u8; PAGE];
                    self.f
                        .read_exact_at(&mut buf, u64::from(first_page + p) * PAGE as u64)?;
                    if !layout::page_intact(&buf) {
                        return Err(SegError::Corrupt("overflow page crc"));
                    }
                    let take = OVERFLOW_CAP.min(total_len as usize - out.len());
                    out.extend_from_slice(&buf[..take]);
                }
                if out.len() != total_len as usize {
                    return Err(SegError::Corrupt("overflow run short"));
                }
                Ok(out)
            }
        }
    }
}

/// Ascending `(key, payload)` iterator over a closed range.
pub struct RangeIter<'a> {
    seg: &'a Seg,
    page_ix: usize,
    slot: u16,
    lo: Vec<u8>,
    hi: Vec<u8>,
    page: Option<Vec<u8>>,
    done: bool,
}

impl Iterator for RangeIter<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>), SegError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            if self.page.is_none() {
                if self.page_ix >= self.seg.fences.len() {
                    self.done = true;
                    return None;
                }
                match self.seg.read_page(self.page_ix) {
                    Ok(p) => self.page = Some(p),
                    Err(e) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                }
                self.slot = 0;
            }
            let page = self.page.as_ref().expect("just set");
            if self.slot >= layout::page_slots(page) {
                self.page = None;
                self.page_ix += 1;
                continue;
            }
            let off = layout::slot_offset(page, self.slot);
            self.slot += 1;
            let Some(cell) = layout::read_cell(page, off) else {
                self.done = true;
                return Some(Err(SegError::Corrupt("cell shape")));
            };
            let k = cell.key();
            if k > self.hi.as_slice() {
                self.done = true;
                return None;
            }
            if k < self.lo.as_slice() {
                continue;
            }
            let key = k.to_vec();
            return Some(self.seg.materialize(cell).map(|p| (key, p)));
        }
    }
}

//! Building a segment: append in key order, page as you go, seal with
//! the footer. The builder owns the only write path this crate has —
//! a sealed segment is never touched again.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::layout::{
    self, OVERFLOW_CAP, PAGE, PAGE_BUDGET, PAGE_CRC, PAGE_HDR, TRAILER,
};
use crate::{SegError, SegMeta};

/// Streaming builder. Records must arrive in strictly ascending key
/// order (the reader's binary search is the reason); a violation is a
/// refusal, not a sort.
pub struct SegBuilder {
    w: BufWriter<File>,
    page: Box<[u8; PAGE]>,
    /// Bytes of cells written into the current page (after the header).
    used: usize,
    /// Cell offsets of the current page, appended to the tail at seal.
    slots: Vec<u16>,
    /// Pages written so far (data + overflow, in file order).
    pages_written: u32,
    /// (page_index, first_key) per data page — the fence table.
    fences: Vec<(u32, Vec<u8>)>,
    records: u64,
    min_key: Vec<u8>,
    last_key: Vec<u8>,
}

impl SegBuilder {
    /// Start a segment at `path` (truncating any leftover — a partial
    /// build is garbage by contract; the manifest above this crate is
    /// what makes a segment real).
    pub fn create(path: &Path) -> Result<Self, SegError> {
        let f = File::create(path)?;
        Ok(Self {
            w: BufWriter::with_capacity(1 << 20, f),
            page: Box::new([0u8; PAGE]),
            used: 0,
            slots: Vec::new(),
            pages_written: 0,
            fences: Vec::new(),
            records: 0,
            min_key: Vec::new(),
            last_key: Vec::new(),
        })
    }

    /// Append one record. Keys strictly ascend; equal keys are refused
    /// (a segment is a map, not a multimap — the caller disambiguates
    /// with its own key suffix when it needs duplicates).
    pub fn push(&mut self, key: &[u8], payload: &[u8]) -> Result<(), SegError> {
        if self.records > 0 && key <= self.last_key.as_slice() {
            return Err(SegError::Unsorted);
        }
        let inline = layout::inline_cell_len(key.len(), payload.len());
        // A cell must fit a page together with its slot entry.
        if inline + 2 <= PAGE_BUDGET {
            self.push_cell(key, |page, off| {
                layout::write_inline_cell(page, off, key, payload)
            })?;
        } else {
            self.push_overflow(key, payload)?;
        }
        if self.records == 0 {
            self.min_key = key.to_vec();
        }
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.records += 1;
        Ok(())
    }

    /// Seal: flush the open page, write the footer and trailer, fsync.
    pub fn finish(mut self) -> Result<SegMeta, SegError> {
        if self.slots.is_empty() && self.records == 0 {
            return Err(SegError::Corrupt("empty segment refused"));
        }
        self.seal_current_page()?;
        let footer = layout::encode_footer(
            self.records,
            self.pages_written,
            &self.min_key,
            &self.last_key,
            &self.fences,
        );
        let footer_off = u64::from(self.pages_written) * PAGE as u64;
        self.w.write_all(&footer)?;
        let mut trailer = [0u8; TRAILER];
        trailer[0..8].copy_from_slice(&footer_off.to_le_bytes());
        trailer[8..12].copy_from_slice(&(footer.len() as u32).to_le_bytes());
        trailer[12..16].copy_from_slice(&layout::MAGIC.to_le_bytes());
        self.w.write_all(&trailer)?;
        self.w.flush()?;
        self.w.get_ref().sync_all()?;
        Ok(SegMeta {
            records: self.records,
            data_pages: self.fences.len() as u32,
            min_key: self.min_key,
            max_key: self.last_key,
        })
    }

    /// Place one cell of `len(page,off)->written` into the current
    /// page, sealing and rolling to a fresh page when it cannot fit.
    fn push_cell(
        &mut self,
        key: &[u8],
        write: impl Fn(&mut [u8], usize) -> usize,
    ) -> Result<(), SegError> {
        let projected = |cell: usize, slots: usize| PAGE_HDR + cell + 2 * (slots + 1);
        // Probe the cell size against a scratch bound: writers report
        // their exact length, so write into the page only when it fits.
        let cell_len = {
            // Worst case both cell kinds: recompute exactly.
            let mut probe = [0u8; PAGE];
            write(&mut probe, 0)
        };
        if self.used + projected(cell_len, self.slots.len()) > PAGE_BUDGET + PAGE_HDR {
            self.seal_current_page()?;
        }
        if self.slots.is_empty() {
            self.fences.push((self.pages_written, key.to_vec()));
        }
        let off = PAGE_HDR + self.used;
        let written = write(&mut self.page[..], off);
        self.slots.push(off as u16);
        self.used += written;
        Ok(())
    }

    /// Spill `payload` into overflow pages, then place the pointer cell.
    fn push_overflow(&mut self, key: &[u8], payload: &[u8]) -> Result<(), SegError> {
        // The pointer cell must itself fit; give it a fresh page if not.
        let cell = layout::overflow_cell_len(key.len());
        if PAGE_HDR + self.used + cell + 2 * (self.slots.len() + 1) > PAGE_BUDGET + PAGE_HDR {
            self.seal_current_page()?;
        }
        // Overflow pages follow the CURRENT data page in file order —
        // seal it first so page indices stay sequential.
        self.seal_current_page()?;
        let first_page = self.pages_written;
        let n_pages = payload.len().div_ceil(OVERFLOW_CAP) as u32;
        for chunk in payload.chunks(OVERFLOW_CAP) {
            let mut p = [0u8; PAGE];
            p[..chunk.len()].copy_from_slice(chunk);
            let crc = kevy_sys::checksum::crc32c(&p[..PAGE - PAGE_CRC]);
            p[PAGE - PAGE_CRC..].copy_from_slice(&crc.to_le_bytes());
            self.w.write_all(&p)?;
            self.pages_written += 1;
        }
        let total = payload.len() as u32;
        self.push_cell(key, move |page, off| {
            layout::write_overflow_cell(page, off, key, total, first_page, n_pages)
        })
    }

    /// Seal and emit the current page, if it holds anything.
    fn seal_current_page(&mut self) -> Result<(), SegError> {
        if self.slots.is_empty() {
            return Ok(());
        }
        // Slot directory packs backward from the CRC.
        for (i, s) in self.slots.iter().enumerate() {
            let pos = PAGE - PAGE_CRC - 2 * (i + 1);
            self.page[pos..pos + 2].copy_from_slice(&s.to_le_bytes());
        }
        layout::seal_page(&mut self.page, self.slots.len() as u16);
        self.w.write_all(&self.page[..])?;
        self.pages_written += 1;
        self.page.fill(0);
        self.used = 0;
        self.slots.clear();
        Ok(())
    }
}

//! Byte layout of pages and the footer — pure functions over buffers,
//! shared by the builder (encode) and the reader (decode) so the two
//! cannot disagree.
//!
//! Data page:
//! ```text
//! [n_slots u16][first_overflow_marker u16]      -- 4-byte header
//! [cell][cell]...                                -- packed forward
//!            ...[slot u16]...[slot u16]          -- packed backward
//! [crc32c u32]                                   -- last 4 bytes
//! ```
//! Inline cell:   [klen u16][plen u32][key][payload]
//! Overflow cell: [klen u16][OVERFLOW u32][key][total_plen u32][first_page u32][n_pages u32]
//!
//! Overflow pages carry raw payload bytes with the same trailing CRC.
//! The footer (encoded by [`encode_footer`]) carries the fence table,
//! min/max keys and counts; the file's final 16 bytes are
//! `[footer_off u64][footer_len u32][MAGIC u32]`.

pub const PAGE: usize = 4096;
/// Trailing per-page CRC.
pub const PAGE_CRC: usize = 4;
/// Data-page header: n_slots + reserved.
pub const PAGE_HDR: usize = 4;
/// Payload length sentinel marking an overflow cell.
pub const OVERFLOW: u32 = u32::MAX;
/// File magic in the trailer.
pub const MAGIC: u32 = 0x4B53_4547; // "KSEG"
/// Bytes of payload an overflow page carries.
pub const OVERFLOW_CAP: usize = PAGE - PAGE_CRC;
/// Trailer at the very end of the file.
pub const TRAILER: usize = 16;

/// Bytes an inline cell of `klen`/`plen` occupies.
pub fn inline_cell_len(klen: usize, plen: usize) -> usize {
    2 + 4 + klen + plen
}

/// Bytes an overflow cell of `klen` occupies (payload lives elsewhere).
pub fn overflow_cell_len(klen: usize) -> usize {
    2 + 4 + klen + 4 + 4 + 4
}

/// Usable cell+slot budget of one data page.
pub const PAGE_BUDGET: usize = PAGE - PAGE_HDR - PAGE_CRC;

/// Seal a data page: write the slot count and the trailing CRC.
/// `used` is bytes of cells written after the header; `slots` are cell
/// offsets (relative to page start), already appended backward by the
/// builder into the page tail.
pub fn seal_page(page: &mut [u8; PAGE], n_slots: u16) {
    page[0..2].copy_from_slice(&n_slots.to_le_bytes());
    let crc = kevy_sys::checksum::crc32c(&page[..PAGE - PAGE_CRC]);
    page[PAGE - PAGE_CRC..].copy_from_slice(&crc.to_le_bytes());
}

/// Verify a page's CRC. `true` = intact.
pub fn page_intact(page: &[u8]) -> bool {
    page.len() == PAGE && {
        let want = u32::from_le_bytes(page[PAGE - PAGE_CRC..].try_into().expect("4 bytes"));
        kevy_sys::checksum::crc32c(&page[..PAGE - PAGE_CRC]) == want
    }
}

/// Slot count of a sealed page.
pub fn page_slots(page: &[u8]) -> u16 {
    u16::from_le_bytes(page[0..2].try_into().expect("2 bytes"))
}

/// The `i`-th cell offset of a sealed page (slots grow backward from
/// the CRC).
pub fn slot_offset(page: &[u8], i: u16) -> usize {
    let pos = PAGE - PAGE_CRC - 2 * (i as usize + 1);
    u16::from_le_bytes(page[pos..pos + 2].try_into().expect("2 bytes")) as usize
}

/// A decoded cell: the key slice and where its payload is.
pub enum Cell<'a> {
    Inline { key: &'a [u8], payload: &'a [u8] },
    Overflow { key: &'a [u8], total_len: u32, first_page: u32, n_pages: u32 },
}

impl<'a> Cell<'a> {
    pub fn key(&self) -> &'a [u8] {
        match self {
            Cell::Inline { key, .. } | Cell::Overflow { key, .. } => key,
        }
    }
}

/// Decode the cell at `off`. `None` = malformed (treated as corrupt by
/// the caller; a CRC-intact page never yields it).
pub fn read_cell(page: &[u8], off: usize) -> Option<Cell<'_>> {
    let klen = u16::from_le_bytes(page.get(off..off + 2)?.try_into().ok()?) as usize;
    let plen = u32::from_le_bytes(page.get(off + 2..off + 6)?.try_into().ok()?);
    let key = page.get(off + 6..off + 6 + klen)?;
    if plen == OVERFLOW {
        let rest = page.get(off + 6 + klen..off + 6 + klen + 12)?;
        Some(Cell::Overflow {
            key,
            total_len: u32::from_le_bytes(rest[0..4].try_into().ok()?),
            first_page: u32::from_le_bytes(rest[4..8].try_into().ok()?),
            n_pages: u32::from_le_bytes(rest[8..12].try_into().ok()?),
        })
    } else {
        let payload = page.get(off + 6 + klen..off + 6 + klen + plen as usize)?;
        Some(Cell::Inline { key, payload })
    }
}

/// Encode an inline cell into `buf` at `off`; returns bytes written.
pub fn write_inline_cell(buf: &mut [u8], off: usize, key: &[u8], payload: &[u8]) -> usize {
    buf[off..off + 2].copy_from_slice(&(key.len() as u16).to_le_bytes());
    buf[off + 2..off + 6].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    buf[off + 6..off + 6 + key.len()].copy_from_slice(key);
    buf[off + 6 + key.len()..off + 6 + key.len() + payload.len()].copy_from_slice(payload);
    inline_cell_len(key.len(), payload.len())
}

/// Encode an overflow cell into `buf` at `off`; returns bytes written.
pub fn write_overflow_cell(
    buf: &mut [u8],
    off: usize,
    key: &[u8],
    total_len: u32,
    first_page: u32,
    n_pages: u32,
) -> usize {
    buf[off..off + 2].copy_from_slice(&(key.len() as u16).to_le_bytes());
    buf[off + 2..off + 6].copy_from_slice(&OVERFLOW.to_le_bytes());
    buf[off + 6..off + 6 + key.len()].copy_from_slice(key);
    let tail = off + 6 + key.len();
    buf[tail..tail + 4].copy_from_slice(&total_len.to_le_bytes());
    buf[tail + 4..tail + 8].copy_from_slice(&first_page.to_le_bytes());
    buf[tail + 8..tail + 12].copy_from_slice(&n_pages.to_le_bytes());
    overflow_cell_len(key.len())
}

/// Footer body: counts, min/max, fence table. CRC'd as a whole; the
/// trailer locates it.
pub fn encode_footer(
    records: u64,
    data_pages: u32,
    min_key: &[u8],
    max_key: &[u8],
    fences: &[(u32, Vec<u8>)], // (page_index, first_key)
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&records.to_le_bytes());
    b.extend_from_slice(&data_pages.to_le_bytes());
    b.extend_from_slice(&(min_key.len() as u32).to_le_bytes());
    b.extend_from_slice(min_key);
    b.extend_from_slice(&(max_key.len() as u32).to_le_bytes());
    b.extend_from_slice(max_key);
    b.extend_from_slice(&(fences.len() as u32).to_le_bytes());
    for (page, key) in fences {
        b.extend_from_slice(&page.to_le_bytes());
        b.extend_from_slice(&(key.len() as u32).to_le_bytes());
        b.extend_from_slice(key);
    }
    let crc = kevy_sys::checksum::crc32c(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    b
}

/// Decode a footer body (inverse of [`encode_footer`]).
/// `None` = corrupt, with the caller naming the refusal.
#[allow(clippy::type_complexity)]
pub fn decode_footer(b: &[u8]) -> Option<(u64, u32, Vec<u8>, Vec<u8>, Vec<(u32, Vec<u8>)>)> {
    if b.len() < 4 {
        return None;
    }
    let (body, crc_bytes) = b.split_at(b.len() - 4);
    let want = u32::from_le_bytes(crc_bytes.try_into().ok()?);
    if kevy_sys::checksum::crc32c(body) != want {
        return None;
    }
    let mut o = 0usize;
    let take = |o: &mut usize, n: usize| -> Option<&[u8]> {
        let s = body.get(*o..*o + n)?;
        *o += n;
        Some(s)
    };
    let records = u64::from_le_bytes(take(&mut o, 8)?.try_into().ok()?);
    let data_pages = u32::from_le_bytes(take(&mut o, 4)?.try_into().ok()?);
    let mklen = u32::from_le_bytes(take(&mut o, 4)?.try_into().ok()?) as usize;
    let min_key = take(&mut o, mklen)?.to_vec();
    let xklen = u32::from_le_bytes(take(&mut o, 4)?.try_into().ok()?) as usize;
    let max_key = take(&mut o, xklen)?.to_vec();
    let nf = u32::from_le_bytes(take(&mut o, 4)?.try_into().ok()?) as usize;
    let mut fences = Vec::with_capacity(nf);
    for _ in 0..nf {
        let page = u32::from_le_bytes(take(&mut o, 4)?.try_into().ok()?);
        let klen = u32::from_le_bytes(take(&mut o, 4)?.try_into().ok()?) as usize;
        fences.push((page, take(&mut o, klen)?.to_vec()));
    }
    (o == body.len()).then_some((records, data_pages, min_key, max_key, fences))
}

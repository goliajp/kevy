//! The staging ring: a small shared file mapping that appends land in
//! before they reach the AOF. A store into a shared mapping is in the page
//! cache the moment it is made, so a process that is killed keeps every
//! append that returned — with no syscall on the append path. A drain
//! later copies the records into the AOF with `write()`.
//!
//! Layout: one header page, then `cap` data bytes used as a ring over a
//! monotonic logical offset. Records are byte-for-byte AOF v2 records, so
//! a drain is a plain copy. A record never straddles the end of the data
//! area: when it does not fit, an 8-byte wrap marker (length 0, CRC 0 —
//! never a valid record) fills the gap, or nothing does when fewer than 8
//! bytes remain, and the record starts again at the beginning.
//!
//! The header's state — which log, how long, how far drained — is three
//! words that must move together: a kill between writing a new length and
//! a new drain offset would replay records the log already holds. So the
//! state lives in two slots, a new one is written into the slot not in use,
//! and one aligned store of the selector publishes it.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use kevy_sys::FileMap;

use crate::crc32c::crc32c;
use crate::record::{MAX_RECORD, RECORD_HEADER};

const MAGIC: &[u8; 8] = b"KEVYSTG2";
/// The header page; the data area starts right after it.
pub(crate) const HEADER: usize = 4096;
const OFF_CAP: usize = 8;
const OFF_COMMIT: usize = 16;
const OFF_SELECT: usize = 24;
const OFF_SLOTS: usize = 32;
/// A slot: the log's id, its length, the drain offset.
const SLOT: usize = 24;

/// What a ring's header said: which log it continues, how long that log
/// was when the ring was last drained into it, and the ring's two
/// logical offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StageHead {
    /// The id of the AOF this ring continues (see `Aof::log_id`).
    pub(crate) log_id: u64,
    /// The AOF's length right after the last drain.
    pub(crate) aof_len: u64,
    /// Everything before this logical offset is in the AOF.
    pub(crate) drained: u64,
    /// Everything before this logical offset was appended and returned.
    pub(crate) commit: u64,
}

/// An open staging ring.
#[derive(Debug)]
pub(crate) struct StageRing {
    map: FileMap,
    cap: u64,
}

impl StageRing {
    /// Open an existing ring as it was left, for recovery. `None` when
    /// there is no file or it does not hold a ring.
    pub(crate) fn open_existing(path: &Path) -> io::Result<Option<(StageRing, StageHead)>> {
        let file = match File::options().read(true).write(true).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        if len <= HEADER as u64 {
            return Ok(None);
        }
        let ring = StageRing { map: FileMap::map(&file, len as usize)?, cap: len - HEADER as u64 };
        if ring.bytes(0, 8) != MAGIC || ring.word(OFF_CAP) != ring.cap {
            return Ok(None);
        }
        let head = ring.head();
        Ok(Some((ring, head)))
    }

    /// Create (or reset) the ring at `path` with `cap` data bytes, empty
    /// and continuing the log `log_id` at length `aof_len`. A new file is
    /// written through once with zeros, so every later store into it lands
    /// on blocks the filesystem has already allocated.
    pub(crate) fn create(
        path: &Path,
        cap: u64,
        log_id: u64,
        aof_len: u64,
    ) -> io::Result<StageRing> {
        assert!(cap.is_power_of_two() && cap >= 64 * 1024, "ring capacity {cap}");
        let total = HEADER as u64 + cap;
        let mut file =
            File::options().read(true).write(true).create(true).truncate(false).open(path)?;
        if file.metadata()?.len() != total {
            file.set_len(0)?;
            let zeros = vec![0u8; 64 * 1024];
            let mut left = total as usize;
            while left > 0 {
                let n = left.min(zeros.len());
                file.write_all(&zeros[..n])?;
                left -= n;
            }
            file.sync_all()?;
        }
        let mut ring = StageRing { map: FileMap::map(&file, total as usize)?, cap };
        ring.put_word(OFF_CAP, cap);
        ring.put_slot(0, [log_id, aof_len, 0]);
        ring.select_cell().store(0, Ordering::Release);
        ring.commit_cell().store(0, Ordering::Release);
        ring.put_bytes(0, MAGIC);
        Ok(ring)
    }

    /// The header as it stands.
    pub(crate) fn head(&self) -> StageHead {
        let at = OFF_SLOTS + SLOT * (self.select_cell().load(Ordering::Acquire) & 1) as usize;
        StageHead {
            log_id: self.word(at),
            aof_len: self.word(at + 8),
            drained: self.word(at + 16),
            commit: self.commit_cell().load(Ordering::Acquire),
        }
    }

    /// The data area's size in bytes.
    pub(crate) fn cap(&self) -> u64 {
        self.cap
    }

    /// Append one record of `len` bytes, written by `fill` into the slot it
    /// is handed. `false`, with nothing written, when the undrained bytes
    /// leave no room; the caller drains and asks again. A record larger
    /// than the ring never fits.
    pub(crate) fn push(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) -> bool {
        let h = self.head();
        let at = h.commit % self.cap;
        let to_end = self.cap - at;
        let skip = if (len as u64) > to_end { to_end } else { 0 };
        if (h.commit - h.drained) + skip + len as u64 > self.cap {
            return false;
        }
        if skip >= RECORD_HEADER as u64 {
            self.put_bytes(HEADER + at as usize, &[0u8; RECORD_HEADER]);
        }
        // a record that did not fit before the end starts at offset 0
        let start = (h.commit + skip) % self.cap;
        fill(self.map.bytes_mut(HEADER + start as usize, len));
        self.commit_cell().store(h.commit + skip + len as u64, Ordering::Release);
        true
    }

    /// Call `each` with the undrained records, as runs of whole records in
    /// logical order — the bytes a drain writes to the AOF. Returns the
    /// logical offset the runs end at.
    pub(crate) fn for_each_pending(&self, mut each: impl FnMut(&[u8])) -> u64 {
        let h = self.head();
        let mut run: Option<(u64, u64)> = None;
        let mut pos = h.drained;
        while let Some((start, len)) = self.record_at(pos, h.commit) {
            match run {
                Some((s, l)) if s + l == start => run = Some((s, l + len)),
                Some((s, l)) => {
                    each(self.logical(s, l));
                    run = Some((start, len));
                }
                None => run = Some((start, len)),
            }
            pos = start + len;
        }
        if let Some((s, l)) = run {
            each(self.logical(s, l));
        }
        h.commit
    }

    /// Record a drain: the records before `to` are in the AOF, which is now
    /// `aof_len` bytes long and has the id `log_id`.
    pub(crate) fn mark_drained(&mut self, to: u64, aof_len: u64, log_id: u64) {
        self.publish([log_id, aof_len, to]);
    }

    /// Point the ring at a new log (a rewrite or reset replaced the file):
    /// everything committed counts as drained.
    pub(crate) fn rebase(&mut self, log_id: u64, aof_len: u64) {
        let commit = self.head().commit;
        self.publish([log_id, aof_len, commit]);
    }

    /// Write `state` into the slot not in use, then select it.
    fn publish(&mut self, state: [u64; 3]) {
        let next = (self.select_cell().load(Ordering::Acquire) & 1) ^ 1;
        self.put_slot(next as usize, state);
        self.select_cell().store(next, Ordering::Release);
    }

    /// A kill between writing a slot and selecting it, for tests.
    #[cfg(test)]
    pub(crate) fn write_unselected(&mut self, state: [u64; 3]) {
        let next = (self.select_cell().load(Ordering::Acquire) & 1) ^ 1;
        self.put_slot(next as usize, state);
    }

    fn put_slot(&mut self, slot: usize, state: [u64; 3]) {
        for (i, w) in state.into_iter().enumerate() {
            self.put_word(OFF_SLOTS + SLOT * slot + 8 * i, w);
        }
    }

    /// The next record at or after logical `pos` and before `end`, skipping
    /// a wrap marker or a tail too short to hold one: `(start, len)` of the
    /// whole record. `None` at `end`, or at bytes that are not a record.
    pub(crate) fn record_at(&self, pos: u64, end: u64) -> Option<(u64, u64)> {
        let mut pos = pos;
        if pos >= end {
            return None;
        }
        let to_end = self.cap - pos % self.cap;
        if to_end < RECORD_HEADER as u64 || self.header_at(pos) == (0, 0) {
            pos += to_end;
        }
        if pos + RECORD_HEADER as u64 > end {
            return None;
        }
        let (len, _) = self.header_at(pos);
        let whole = RECORD_HEADER as u64 + u64::from(len);
        if len == 0 || len > MAX_RECORD || pos + whole > end || whole > self.cap - pos % self.cap {
            return None;
        }
        Some((pos, whole))
    }

    /// Whether the record at `start` of `len` bytes carries its checksum.
    pub(crate) fn record_intact(&self, start: u64, len: u64) -> bool {
        let rec = self.logical(start, len);
        let crc = u32::from_le_bytes(rec[4..8].try_into().expect("a record has an 8-byte header"));
        crc32c(&rec[RECORD_HEADER..]) == crc
    }

    /// `len` bytes at logical `start`, which never cross the end of the
    /// data area (records do not straddle it).
    pub(crate) fn logical(&self, start: u64, len: u64) -> &[u8] {
        self.bytes(HEADER + (start % self.cap) as usize, len as usize)
    }

    fn header_at(&self, pos: u64) -> (u32, u32) {
        let h = self.logical(pos, RECORD_HEADER as u64);
        let len = u32::from_le_bytes(h[..4].try_into().expect("four bytes"));
        let crc = u32::from_le_bytes(h[4..].try_into().expect("four bytes"));
        (len, crc)
    }

    fn bytes(&self, off: usize, len: usize) -> &[u8] {
        self.map.bytes(off, len)
    }

    fn put_bytes(&mut self, off: usize, src: &[u8]) {
        self.map.bytes_mut(off, src.len()).copy_from_slice(src);
    }

    fn word(&self, off: usize) -> u64 {
        u64::from_le_bytes(self.bytes(off, 8).try_into().expect("eight bytes"))
    }

    fn put_word(&mut self, off: usize, v: u64) {
        self.put_bytes(off, &v.to_le_bytes());
    }

    fn commit_cell(&self) -> &AtomicU64 {
        self.map.atomic_u64(OFF_COMMIT)
    }

    fn select_cell(&self) -> &AtomicU64 {
        self.map.atomic_u64(OFF_SELECT)
    }
}

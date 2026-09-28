//! Mapped appends: the log file is extended ahead of the writes in chunks,
//! each chunk is mapped, and an append is a copy into the mapping — no
//! `write()` and no user-space buffer. A store into a shared mapping is in
//! the page cache the moment it is made, so a killed process keeps every
//! append that returned. The file ends in zeros past the last record, the
//! part of the last chunk no record reached; replay stops there cleanly
//! and the open cuts it off.
//!
//! Only worth it where a fresh mapped page is cheap: on Apple platforms
//! a 4 KiB append this way costs about a tenth of going through `write()`,
//! while on Linux the page fault makes it slower than `write()`.

use kevy_sys::{FileMap, MapSync};
use std::fs::File;
use std::io::{self, Write};

use crate::aof::Aof;

const FIRST_CHUNK: u64 = 4 << 20;
const LAST_CHUNK: u64 = 64 << 20;
/// Mappings start on a multiple of this, which is a whole number of pages
/// on every platform (4 KiB and 16 KiB pages alike).
const ALIGN: u64 = 64 * 1024;

/// A log's mapped chunks and where its records end.
#[derive(Debug)]
pub(crate) struct Mapped {
    file: File,
    /// Each chunk and the file offset it starts at, in file order.
    chunks: Vec<(u64, FileMap)>,
    /// The logical end: where the next record goes.
    end: u64,
    /// The file's length: the end of the last chunk.
    phys: u64,
    next_chunk: u64,
}

impl Mapped {
    /// Map `file` for appends after its first `end` bytes.
    fn open(file: File, end: u64) -> io::Result<Mapped> {
        let mut m = Mapped { file, chunks: Vec::new(), end, phys: end, next_chunk: FIRST_CHUNK };
        let start = end - end % ALIGN;
        m.extend_from(start, FIRST_CHUNK)?;
        Ok(m)
    }

    fn extend_from(&mut self, start: u64, len: u64) -> io::Result<()> {
        self.file.set_len(start + len)?;
        let map = FileMap::map_at(&self.file, start, len as usize)?;
        self.chunks.push((start, map));
        self.phys = start + len;
        self.next_chunk = (len * 2).min(LAST_CHUNK);
        Ok(())
    }

    /// Sync handles for the chunks, for a sync that runs after the lock is
    /// released: each chunk stays mapped until its last holder lets go.
    pub(crate) fn sync_handles(&self) -> Vec<MapSync> {
        self.chunks.iter().map(|(_, m)| m.sync_handle()).collect()
    }

    /// Put the mapped pages on the medium: `msync` each chunk, then the
    /// file's own sync (`F_FULLFSYNC` on Apple platforms).
    pub(crate) fn sync(&self) -> io::Result<()> {
        for (_, m) in &self.chunks {
            m.sync()?;
        }
        self.file.sync_data()
    }

    /// Put the mapped pages on their way to the file, unmap, and cut the
    /// file back to its last record.
    fn close(self) -> io::Result<()> {
        let synced = self.chunks.iter().try_for_each(|(_, m)| m.sync());
        drop(self);
        synced
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        self.chunks.clear();
        // a failed cut leaves the zero tail, which the next open removes
        #[expect(clippy::let_underscore_must_use, reason = "replay handles the tail it leaves")]
        let _ = self.file.set_len(self.end);
    }
}

/// A chunk's sync handle, as an off-lock sync carries it.
pub(crate) type MapHandle = MapSync;

/// `msync` each handle's chunk.
pub(crate) fn sync_handles(maps: &[MapHandle]) -> io::Result<()> {
    maps.iter().try_for_each(MapSync::sync)
}

impl Write for Mapped {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.end == self.phys {
            let len = self.next_chunk;
            self.extend_from(self.phys, len)?;
        }
        let (start, map) = self.chunks.last_mut().expect("a mapped log has a chunk");
        let at = (self.end - *start) as usize;
        let n = buf.len().min(map.len() - at);
        map.bytes_mut(at, n).copy_from_slice(&buf[..n]);
        self.end += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Aof {
    /// Append by mapping from here on: the rest of this log's life, until a
    /// switch to `Always` turns it back. Only a v2 log that is not in queued
    /// mode maps; call it after the log was replayed and any staging ring it
    /// had was settled, so no record is still on its way through `write()`.
    pub fn map_appends(&mut self) -> io::Result<bool> {
        let can = self.format == crate::AofFormat::V2
            && self.queue.is_none()
            && self.stage.is_none()
            && !matches!(self.fsync, crate::Fsync::Always);
        self.maps = can;
        if can && self.mapped.is_none() {
            self.file.flush()?;
            let end = self.file.get_ref().metadata()?.len();
            // a shared writable mapping needs a descriptor open for reading
            // too; the log's own is append-only
            let file = File::options().read(true).write(true).open(&self.path)?;
            self.mapped = Some(Mapped::open(file, end)?);
        }
        Ok(can)
    }

    /// Whether appends go into a mapping.
    pub fn maps_appends(&self) -> bool {
        self.mapped.is_some()
    }

    /// Sync the log's bytes to the medium: the mapped chunks first, when
    /// there are any, then the file.
    pub(crate) fn sync_file(&self) -> io::Result<()> {
        match &self.mapped {
            Some(m) => m.sync(),
            None => self.file.get_ref().sync_data(),
        }
    }

    /// Handles for an off-lock sync of the mapped chunks (none unmapped).
    pub(crate) fn map_handles(&self) -> Vec<MapHandle> {
        self.mapped.as_ref().map(Mapped::sync_handles).unwrap_or_default()
    }

    /// Stop mapping for good (a switch to `Always`): later appends go
    /// through `write()` again.
    pub(crate) fn stop_mapping(&mut self) -> io::Result<()> {
        self.maps = false;
        self.unmap()
    }

    /// Unmap and cut the file to its records before the file is renamed
    /// over, truncated or synced through the write path.
    pub(crate) fn unmap(&mut self) -> io::Result<()> {
        match self.mapped.take() {
            Some(m) => m.close(),
            None => Ok(()),
        }
    }

    /// The file was replaced or emptied: point the staging ring at it, or
    /// map its tail.
    pub(crate) fn after_file_change(&mut self) -> io::Result<()> {
        self.rebase_stage()?;
        self.remap()
    }

    /// Map again after the file was replaced or emptied.
    pub(crate) fn remap(&mut self) -> io::Result<()> {
        if !self.maps {
            return Ok(());
        }
        self.unmap()?;
        self.map_appends().map(|_| ())
    }
}

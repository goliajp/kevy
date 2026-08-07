//! Owner-driven compaction: victim selection, the resumable
//! bounded-batch drain, and the sequential record scan. Split from
//! `lib.rs` at the natural seam (nothing here runs on the append or
//! read fast paths).

use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::{CompactOwner, HEADER, MAX_BODY, Vlog, VlogFile, VlogRef, bad};

/// Resumable position within the file currently being compacted.
pub(crate) struct CompactCursor {
    /// Pinned so record reads never borrow `Vlog::files` (freeing the
    /// `&mut self` that `append` needs for the survivor).
    handle: Arc<VlogFile>,
    file_id: u32,
    bytes: u64,
    offset: u64,
}

impl Vlog {
    /// Is there compaction work outstanding at `live_pct`? — a cursor is
    /// mid-file, or some sealed file is below the live ratio. Lets the
    /// caller drive [`Self::compact_step`] on the tick only when there is
    /// something to do (the scan is O(files), no IO).
    pub fn compaction_pending(&self, live_pct: u32) -> bool {
        if self.compaction.is_some() {
            return true;
        }
        let sealed = self.files.len().saturating_sub(1);
        self.files[..sealed]
            .iter()
            .any(|s| s.bytes > 0 && s.live.saturating_mul(100) < s.bytes.saturating_mul(u64::from(live_pct)))
    }

    /// Compact SEALED files whose live ratio fell below `live_pct`
    /// percent (the active file is never compacted), doing at most
    /// `budget` records of work before returning — so a single call
    /// never blocks the caller for a whole-file rewrite. A victim is
    /// drained across successive calls via [`Vlog::compaction`]; it stays
    /// in `files` (readable, pin-safe) until fully drained, then
    /// unlink-on-last-pin + one epoch bump (unchanged retirement
    /// semantics — a ref only becomes invalid when its file is deleted,
    /// which is still atomic per file). Returns records processed this
    /// call (0 = nothing to compact). Call in a loop to drain fully.
    pub fn compact_step(
        &mut self,
        live_pct: u32,
        owner: &mut dyn CompactOwner,
        budget: usize,
    ) -> io::Result<usize> {
        self.compact_step_capped(live_pct, owner, budget, u32::MAX)
    }

    /// Drop fully-dead sealed files (FLUSHALL / full supersession) scan-
    /// free, then arm [`Vlog::compaction`] on the next live-but-below-
    /// threshold victim. Only files with `id < id_below` are eligible.
    /// Returns `true` if a victim was armed, `false` if none remain.
    fn begin_compaction(&mut self, live_pct: u32, id_below: u32) -> bool {
        let sealed = self.files.len().saturating_sub(1);
        let dead: Vec<u32> = self.files[..sealed]
            .iter()
            .filter(|s| s.bytes > 0 && s.live == 0 && s.handle.id < id_below)
            .map(|s| s.handle.id)
            .collect();
        for id in dead {
            if let Some(pos) = self.files.iter().position(|s| s.handle.id == id) {
                let state = self.files.remove(pos);
                state.handle.delete_on_drop.store(true, Ordering::Release);
                self.epoch += 1;
            }
        }
        let sealed = self.files.len().saturating_sub(1);
        let Some(v) = self.files[..sealed].iter().find(|s| {
            s.handle.id < id_below
                && s.live > 0
                && s.live.saturating_mul(100) < s.bytes.saturating_mul(u64::from(live_pct))
        }) else {
            return false;
        };
        self.compaction = Some(CompactCursor {
            handle: Arc::clone(&v.handle),
            file_id: v.handle.id,
            bytes: v.bytes,
            offset: 0,
        });
        true
    }

    /// [`Self::compact_step`] restricted to files with `id < id_below`.
    /// The full-drain wrapper passes a ceiling so files newly SEALED by
    /// this pass's own survivor appends (always higher ids) are never
    /// re-selected — that reproduces the one-shot victim set of the
    /// original compactor and guarantees termination at ANY `live_pct`
    /// (at `live_pct > 100` even a fully-live file qualifies, so without
    /// the ceiling survivors would ping-pong between files forever).
    fn compact_step_capped(
        &mut self,
        live_pct: u32,
        owner: &mut dyn CompactOwner,
        budget: usize,
        id_below: u32,
    ) -> io::Result<usize> {
        if self.compaction.is_none() && !self.begin_compaction(live_pct, id_below) {
            return Ok(0);
        }
        let mut cur = self.compaction.take().expect("set above");
        let mut done = 0usize;
        while cur.offset < cur.bytes && done < budget {
            let (key, payload, body_len) = read_record(&cur.handle, cur.offset)?;
            let old = VlogRef { file_id: cur.file_id, offset: cur.offset, len: body_len };
            if owner.is_live(&key, old) {
                let new = self.append_high(&key, &payload)?;
                owner.moved(&key, old, new);
            }
            cur.offset += HEADER + u64::from(body_len);
            done += 1;
        }
        if cur.offset >= cur.bytes {
            if let Some(pos) = self.files.iter().position(|s| s.handle.id == cur.file_id) {
                let state = self.files.remove(pos);
                state.handle.delete_on_drop.store(true, Ordering::Release);
            }
            self.epoch += 1;
            // cursor dropped — next call picks the next victim.
        } else {
            self.compaction = Some(cur);
        }
        Ok(done)
    }

    /// Drain compaction fully at `live_pct` (test / single-threaded bulk
    /// paths). Returns retired-file count.
    pub fn compact_below(&mut self, live_pct: u32, owner: &mut dyn CompactOwner) -> io::Result<usize> {
        let epoch0 = self.epoch;
        // Ceiling = every file that exists NOW; survivor appends create
        // higher-id files this pass must not re-select (else it never
        // terminates at live_pct > 100).
        let ceiling = self.next_id;
        while self.compact_step_capped(live_pct, owner, usize::MAX, ceiling)? > 0 {}
        Ok((self.epoch - epoch0) as usize)
    }
}

/// Sequential-scan read of the record at `offset` (compaction path):
/// `(key, payload, body_len)`.
fn read_record(f: &VlogFile, offset: u64) -> io::Result<(Vec<u8>, Vec<u8>, u32)> {
    let mut header = [0u8; HEADER as usize];
    f.file.read_exact_at(&mut header, offset)?;
    let body_len = u32::from_le_bytes(header[..4].try_into().unwrap());
    if body_len > MAX_BODY {
        return Err(bad(format!("vlog: scan hit absurd body_len {body_len}")));
    }
    let (key, payload) = f.read(VlogRef { file_id: f.id, offset, len: body_len })?;
    Ok((key, payload, body_len))
}

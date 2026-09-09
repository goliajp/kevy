//! Safe batched positional file reads — the cold-hydration
//! primitive: N preads on already-open files submitted as few
//! `io_uring_enter` calls as the SQ allows, waited synchronously.
//! Buffers are owned inside the call, so the `unsafe` SQE plumbing is
//! fully encapsulated (callers under `#![forbid(unsafe_code)]` — the
//! kevy server — stay clean).

use std::io;

use crate::completion::Completion;
use crate::ring::IoUring;

/// One positional read: `len` bytes at `offset` on `fd`. The fd must
/// stay open until the call returns (the caller's pin/ownership).
#[derive(Clone, Copy, Debug)]
pub struct FileRead {
    /// Open file descriptor to read from.
    pub fd: i32,
    /// Byte offset of the read within the file.
    pub offset: u64,
    /// Bytes to read — the completion must deliver exactly this many.
    pub len: u32,
}

/// Check one completion against the read that asked for it, recording
/// the first mismatch.
///
/// `for_each_completion` drains the whole queue, so a completion
/// belonging to anything else on this ring arrives here too. Its
/// `user_data` used to index `reads` directly: kevy's reactor tags its
/// own with `OP_* << 60 | cid`, which as an index is astronomically out
/// of bounds — a panic — and a smaller foreign value would have
/// silently validated the wrong read.
fn reap_one(c: Completion, reads: &[FileRead], bad: &mut Option<(usize, i32)>) {
    if c.user_data & FILE_BATCH_TAG_MASK != FILE_BATCH_TAG {
        return;
    }
    let i = (c.user_data & !FILE_BATCH_TAG_MASK) as usize;
    let Some(r) = reads.get(i) else { return };
    if c.res != r.len as i32 && bad.is_none() {
        *bad = Some((i, c.res));
    }
}

/// Marks this call's own submissions. `for_each_completion` drains the
/// whole queue, so without a tag a completion from any other submitter
/// on the same ring is read as one of ours.
const FILE_BATCH_TAG: u64 = 0xF11E_B000_0000_0000;
/// The bits `FILE_BATCH_TAG` occupies; the rest carry the index.
const FILE_BATCH_TAG_MASK: u64 = 0xFFFF_FFFF_0000_0000;

impl IoUring {
    /// Queue as many of `reads[from..]` as the submission queue accepts,
    /// returning how many. Zero means the queue had no free slot at all.
    fn fill_chunk(&mut self, reads: &[FileRead], bufs: &mut [Vec<u8>], from: usize) -> u32 {
        let mut n = 0u32;
        for i in from..reads.len() {
            let (r, buf) = (&reads[i], &mut bufs[i]);
            // SAFETY: `buf` lives in `bufs`, which the caller neither
            // resizes nor drops until this chunk's completions are
            // reaped (leaked on the error path).
            let ok = unsafe {
                self.prep_read_at(
                    r.fd,
                    buf.as_mut_ptr(),
                    r.len,
                    r.offset,
                    FILE_BATCH_TAG | i as u64,
                )
            };
            if !ok {
                break; // SQ full — submit this chunk first
            }
            n += 1;
        }
        n
    }

    /// Read every entry of `reads`, returning the buffers in input
    /// order plus the number of `io_uring_enter` submissions made
    /// (batches larger than the SQ chunk across several). Each read
    /// must complete in FULL — a short or failed pread is an error
    /// (the caller reads records it wrote itself; partial data is a
    /// bug, not a case to handle).
    pub fn read_file_batch(&mut self, reads: &[FileRead]) -> io::Result<(Vec<Vec<u8>>, u64)> {
        let mut bufs: Vec<Vec<u8>> = reads.iter().map(|r| vec![0u8; r.len as usize]).collect();
        let mut submissions = 0u64;
        let mut done = 0usize;
        while done < reads.len() {
            let in_chunk = self.fill_chunk(reads, &mut bufs, done);
            if in_chunk == 0 {
                // `debug_assert!` here compiled away in release, and the
                // release behaviour was an infinite loop rather than a
                // panic: `submit_and_wait(0)` reaps nothing, `done`
                // advances by nothing, and the same full SQ is retried
                // for ever. It is reachable whenever this ring has
                // another submitter, which nothing prevents.
                std::mem::forget(bufs);
                return Err(io::Error::other(
                    "submission queue has no free slot for a batched read",
                ));
            }
            if let Err(e) = self.submit_and_wait(in_chunk) {
                // The kernel may still own the prepped buffers; leaking
                // them is the only sound exit (error here is a process
                // bug by the caller's doctrine — it aborts anyway).
                std::mem::forget(bufs);
                return Err(e);
            }
            submissions += 1;
            let mut bad: Option<(usize, i32)> = None;
            self.for_each_completion(|c| reap_one(c, reads, &mut bad));
            if let Some((i, res)) = bad {
                return Err(io::Error::other(format!(
                    "batched file read {i} returned {res}, want {}",
                    reads[i].len
                )));
            }
            done += in_chunk as usize;
        }
        Ok((bufs, submissions))
    }
}

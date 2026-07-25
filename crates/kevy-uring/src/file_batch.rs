//! Safe batched positional file reads — the cold-hydration
//! primitive: N preads on already-open files submitted as few
//! `io_uring_enter` calls as the SQ allows, waited synchronously.
//! Buffers are owned inside the call, so the `unsafe` SQE plumbing is
//! fully encapsulated (callers under `#![forbid(unsafe_code)]` — the
//! kevy server — stay clean).

use std::io;

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

impl IoUring {
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
            let mut in_chunk = 0u32;
            for i in done..reads.len() {
                let (r, buf) = (&reads[i], &mut bufs[i]);
                // SAFETY: `buf` lives in `bufs`, which is neither
                // resized nor dropped until this chunk's completions
                // are reaped below (leaked on the error path).
                let ok = unsafe {
                    self.prep_read_at(r.fd, buf.as_mut_ptr(), r.len, r.offset, i as u64)
                };
                if !ok {
                    break; // SQ full — submit this chunk first
                }
                in_chunk += 1;
            }
            debug_assert!(in_chunk > 0, "an empty SQ accepts at least one SQE");
            if let Err(e) = self.submit_and_wait(in_chunk) {
                // The kernel may still own the prepped buffers; leaking
                // them is the only sound exit (error here is a process
                // bug by the caller's doctrine — it aborts anyway).
                std::mem::forget(bufs);
                return Err(e);
            }
            submissions += 1;
            let mut bad: Option<(usize, i32)> = None;
            self.for_each_completion(|c| {
                let i = c.user_data as usize;
                if c.res != reads[i].len as i32 && bad.is_none() {
                    bad = Some((i, c.res));
                }
            });
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

//! Transaction brackets for the AOF: the begin/commit markers that make
//! a group of appends replay all-or-nothing, plus the group-commit
//! window they pair with. Split from `aof.rs` to keep that file under
//! the 500-LOC house rule.
//!
//! Why markers and not just group commit: group commit defers the
//! *fsync*, but the AOF writes through a 256 KiB buffer, so a longer
//! transaction still hands whole, valid frames to the kernel as that
//! buffer fills — and `kill -9` leaves them there. Measured before these
//! existed: a 20,000-mutation block killed mid-commit replayed 6,393 of
//! them. The markers move "did this transaction finish" out of "how much
//! got flushed" and into the log itself.

use super::Aof;
use crate::Fsync;
use std::io::{self, Write};
use std::time::Instant;

impl Aof {
    /// The two markers that make a group of appends replay atomically.
    ///
    /// Group commit alone only defers the *fsync*; the frames still reach
    /// the kernel whenever the write buffer fills, so a `kill -9` during
    /// a transaction larger than that buffer leaves a durably
    /// half-applied block — measured at 6393/20000 before these markers
    /// existed. Bracketing the frames means replay can tell a completed
    /// transaction from a torn one regardless of size:
    /// [`crate::replay_aof`] buffers everything after a begin marker and
    /// applies it only on seeing the matching commit marker, discarding
    /// the buffer at EOF.
    ///
    /// They ride as ordinary v2 records holding a one-element multibulk,
    /// so the on-disk format is unchanged and older readers see a command
    /// they will reject rather than a corrupt frame. The names start with
    /// a NUL, which no RESP verb can.
    pub const TXN_BEGIN: &'static [u8] = b"\0KEVYTXNBEGIN";
    /// See [`Self::TXN_BEGIN`].
    pub const TXN_COMMIT: &'static [u8] = b"\0KEVYTXNCOMMIT";


    /// Append one of the transaction markers as a bare one-element frame.
    fn append_marker(&mut self, name: &'static [u8]) -> io::Result<()> {
        let mut frame = Vec::with_capacity(name.len() + 24);
        frame.extend_from_slice(b"*1\r\n$");
        frame.extend_from_slice(name.len().to_string().as_bytes());
        frame.extend_from_slice(b"\r\n");
        frame.extend_from_slice(name);
        frame.extend_from_slice(b"\r\n");
        match self.format {
            crate::AofFormat::V2 => {
                // Queued-append mode (AOF offload): the marker rides the
                // queue like every other record — writing it straight to
                // the file here would interleave with in-flight
                // positioned writes and corrupt the log.
                if let Some(q) = &mut self.queue {
                    q.extend_from_slice(&(frame.len() as u32).to_le_bytes());
                    q.extend_from_slice(&crate::crc32c::crc32c(&frame).to_le_bytes());
                    q.extend_from_slice(&frame);
                } else {
                    self.file.write_all(&(frame.len() as u32).to_le_bytes())?;
                    self.file.write_all(&crate::crc32c::crc32c(&frame).to_le_bytes())?;
                    self.file.write_all(&frame)?;
                }
                // A concurrent rewrite's diff buffer must carry the
                // markers too, or the post-rewrite log would replay the
                // bracketed run as independent commands.
                if let Some(tee) = &mut self.rewrite_tee {
                    crate::record::write_record(tee, &frame)?;
                }
                self.size_bytes = self
                    .size_bytes
                    .saturating_add((crate::record::RECORD_HEADER + frame.len()) as u64);
            }
            // v1 has no envelope, so it cannot express a torn-transaction
            // boundary; a v1 log stays exactly as atomic as it was. The
            // first rewrite upgrades it to v2 and it gains this.
            crate::AofFormat::V1 => {}
        }
        Ok(())
    }

    /// Open a group-commit window (no-op unless the policy is `Always`):
    /// subsequent `append`s buffer instead of fsyncing per command, and
    /// the run is bracketed by transaction markers so replay applies it
    /// all-or-nothing. Pair with [`Self::end_group`] **before** sending
    /// the batch's replies.
    #[inline]
    pub fn begin_group(&mut self) {
        self.begin_fsync_window();
        self.in_txn = self.append_marker(Self::TXN_BEGIN).is_ok();
    }

    /// Open ONLY the group-fsync half of the window: `Fsync::Always`
    /// appends buffer until [`Self::end_group`] fsyncs once — no
    /// transaction markers. This is the reactor-batch bracket: a
    /// pipelined read batch is not a transaction (Redis pipelining is
    /// explicitly non-atomic), so bracketing it with BEGIN/COMMIT
    /// records over-promised at ~65 B per single-command batch — the
    /// dominant shape for every non-pipelining client. Real atomic
    /// units (embedded `atomic()`, EXEC) call [`Self::begin_group`].
    #[inline]
    pub fn begin_fsync_window(&mut self) {
        if matches!(self.fsync, Fsync::Always) {
            self.deferred = true;
        }
    }

    /// Close the group-commit window: one `flush()+sync_data()` for the
    /// whole batch (if anything was buffered), then resume per-command
    /// fsync. Must be called before the batch's replies leave the shard.
    #[inline]
    pub fn end_group(&mut self) -> io::Result<()> {
        // Commit marker BEFORE the sync: replay treats a transaction as
        // committed only once this record is present and intact, so it
        // must be inside the same durable run as the frames it closes.
        if self.in_txn {
            self.in_txn = false;
            self.append_marker(Self::TXN_COMMIT)?;
        }
        if self.deferred {
            self.deferred = false;
            // Queued bytes are in the driver's chunk — syncing the file
            // here would durabilize nothing. Leave `dirty` set; the ring
            // fsync is the durability point for queued appends.
            if self.dirty && self.queue.is_none() {
                self.file.flush()?;
                self.file.get_ref().sync_data()?;
                self.dirty = false;
                self.last_sync = Instant::now();
            }
        }
        Ok(())
    }

}

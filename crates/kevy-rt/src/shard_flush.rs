//! The outbound half of the shard transport: cross-core sends (ring push +
//! backlog spill + coalesced wakeups) and connection output flushing. Split
//! out of [`crate::shard`] to keep that file under the 500-LOC house rule —
//! every method here is on the same `impl<C: Commands> Shard<C>`.

use crate::Commands;
use crate::message::Inbound;
use crate::shard::Shard;
use std::io;
use std::sync::atomic::{Ordering, fence};

impl<C: Commands> Shard<C> {
    /// Wake every target enqueued to this iteration that is currently parked.
    /// A spinning peer needs no syscall — it will see the message on its next
    /// poll(0). This is what removes the per-message wakeup under load.
    pub(crate) fn flush_wakes(&mut self) {
        // Fast-path single-shard: pending_wakes is len-nshards; in the common
        // single-shard benchmark this loop runs nshards times even when no
        // wakes are pending. Skip outright when nothing's flagged.
        if !self.pending_wakes.iter().any(|&w| w) {
            return;
        }
        // Close the park/wake race: the SeqCst fence pairs with the
        // matching fence in `Shard::run` after a peer stores `parked=true`.
        // Combined, they guarantee: if our ring push (Release on the
        // outbox's tail, executed earlier this iteration via `send_to`)
        // happens-before this load, AND the peer's parked-store
        // happens-before its post-park drain, then either
        //   (a) the peer's drain sees our push,            OR
        //   (b) our load sees `parked=true` and we send the wake.
        // Loom-verified by `kevy-rt/tests/loom.rs::no_wake_implies_drained`.
        // Without the fence the lost-wake window was bounded by the
        // peer's `PARK_TIMEOUT_MS` (50 ms); the timeout remains as
        // defense-in-depth against missed eventfd writes / OS hiccups.
        fence(Ordering::SeqCst);
        for i in 0..self.pending_wakes.len() {
            if self.pending_wakes[i] {
                self.pending_wakes[i] = false;
                if self.parked[i].load(Ordering::SeqCst) {
                    let _ = self.wakers[i].wake();
                }
            }
        }
    }

    /// Flush connections a PUBLISH appended output to this iteration (epoll path;
    /// the io_uring reactor flushes them via its arm/write loop instead).
    #[inline]
    pub(crate) fn flush_dirty(&mut self) -> io::Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        while let Some(id) = self.dirty.pop() {
            self.flush_conn(id)?;
        }
        Ok(())
    }

    /// Enqueue a message to another shard, marking it for a coalesced wakeup. The
    /// fast path is a lock-free ring push; on a full ring it spills to the local
    /// per-target backlog (preserving order), which `flush_backlog` drains later.
    pub(crate) fn send_to(&mut self, dst: usize, msg: Inbound) {
        if self.backlog[dst].is_empty() {
            match self.outboxes[dst].as_mut() {
                Some(p) => {
                    if let Err(m) = p.push(msg) {
                        self.backlog[dst].push_back(m);
                    }
                }
                // `dst == self.id` has no ring and is never sent to.
                None => return,
            }
        } else {
            // Order: queue behind the existing backlog rather than jumping the ring.
            self.backlog[dst].push_back(msg);
        }
        self.pending_wakes[dst] = true;
    }

    /// Re-push each per-target backlog into its ring (filled when a ring was full
    /// last iteration). Stops at the first target whose ring is still full.
    #[inline]
    pub(crate) fn flush_backlog(&mut self) {
        // Outer-empty short-circuit: in the hot single-shard / no-backlog
        // path this avoids the nshards loop entirely.
        if self.backlog.iter().all(|b| b.is_empty()) {
            return;
        }
        for dst in 0..self.nshards {
            if self.backlog[dst].is_empty() {
                continue;
            }
            let Some(p) = self.outboxes[dst].as_mut() else {
                self.backlog[dst].clear();
                continue;
            };
            while let Some(msg) = self.backlog[dst].pop_front() {
                if let Err(m) = p.push(msg) {
                    self.backlog[dst].push_front(m);
                    break;
                }
                self.pending_wakes[dst] = true;
            }
        }
    }

    /// Write a connection's staged output to its socket: drain until done or
    /// WouldBlock, drop the conn once closing + fully drained, and keep the
    /// poller's write-interest in sync with whether output remains.
    pub(crate) fn flush_conn(&mut self, conn_id: u64) -> io::Result<()> {
        let (close, want_write, fd) = {
            let Some(conn) = self.conns.get_mut(&conn_id) else {
                return Ok(());
            };
            while conn.write_pos < conn.output.len() {
                match conn.sock.write(&conn.output[conn.write_pos..]) {
                    Ok(0) => break,
                    Ok(n) => conn.write_pos += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        conn.closing = true;
                        break;
                    }
                }
            }
            if conn.write_pos == conn.output.len() {
                conn.output.clear();
                conn.write_pos = 0;
            }
            let out_remaining = conn.write_pos < conn.output.len();
            let close = conn.closing && conn.pending.is_empty() && !out_remaining;
            (close, out_remaining, conn.sock.raw())
        };

        if close {
            self.close_conn(conn_id);
            return Ok(());
        }
        if let Some(conn) = self.conns.get_mut(&conn_id)
            && want_write != conn.want_write
        {
            conn.want_write = want_write;
            self.poller.modify(fd, true, want_write)?;
        }
        Ok(())
    }
}

//! Per-shard reactor glue for the replication subsystem: accept new
//! replica connections, drive their read/write handlers, reap closed
//! conns into the slot table, and periodically expire stale slots.
//!
//! Split from [`crate::replication`] (which holds the state machine
//! types + `close()` + handshake parser) so each file stays under the
//! 500-LOC house rule. All methods here are `impl<C: Commands> Shard<C>`.

use crate::Commands;
use crate::replication::{ReplicaConn, ReplicaState, advance_handshake};
use crate::shard::Shard;
use std::io;

/// Maximum bytes a replication conn may buffer before handshake
/// completes. The whole `REPLICATE FROM <offset> ID <id>` fits in
/// ~80 bytes for any realistic id; 4 KiB is generous + catches a
/// misbehaving / hostile peer trying to OOM the primary by holding a
/// handshake half-open.
const HANDSHAKE_MAX_INPUT: usize = 4 * 1024;

/// Maximum in-flight `accept(2)`s per `accept_ready_replication` tick.
/// We drain until `WouldBlock`, but cap to defend against an
/// accept-flood DoS. Real replica counts are < 16 so this is room
/// to spare.
const ACCEPT_BURST_CAP: usize = 64;

/// Cap on the input buffer a streaming replica may accumulate before
/// being dropped. In v1.18.0 there is no replica→primary ACK protocol
/// (sent-offset is taken as acked-offset; real acks land in Phase 1.5
/// with `kevy-elect`), so input is drain-and-discard. The cap protects
/// against a peer dumping arbitrary bytes hoping to bloat memory.
const STREAMING_INPUT_DISCARD_CAP: usize = 64 * 1024;

impl<C: Commands> Shard<C> {
    /// Drain the replication listener — accept until `WouldBlock` or
    /// the burst cap. Each accepted socket goes into `self.replicas`
    /// and gets registered with the poller for readability.
    pub(crate) fn accept_ready_replication(&mut self) -> io::Result<()> {
        let Some(listener) = self.replication_listener.as_ref() else {
            return Ok(());
        };
        for _ in 0..ACCEPT_BURST_CAP {
            match listener.accept() {
                Ok(sock) => {
                    sock.set_nonblocking()?;
                    self.poller.add(sock.raw(), true, false)?;
                    // T1.28.5: capture the replica's peer addr at
                    // accept time so `INFO replication` / `ROLE` can
                    // report it. `peer_addr` errs on a peer that
                    // already vanished — fall back to 0.0.0.0:0,
                    // the connection will reap on the next read.
                    let peer = sock.peer_addr().unwrap_or((
                        std::net::Ipv4Addr::UNSPECIFIED,
                        0,
                    ));
                    self.replicas.push(ReplicaConn::with_peer(sock, peer));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Locate a replica conn by raw fd. Linear scan — replica counts
    /// are < 16; a hashmap would cost more than the few comparisons.
    pub(crate) fn replica_index_by_fd(&self, fd: i32) -> Option<usize> {
        self.replicas.iter().position(|r| r.fd == fd)
    }

    /// Handle readability on a replica conn. In `HandshakePending`,
    /// pull bytes, advance handshake state, queue `+ACK` on success,
    /// `close()` on failure. In `Streaming`, drain-and-discard (no
    /// replica→primary ACK in v1.18). In `AckSent`/`Closed`, ignore.
    pub(crate) fn replica_readable(&mut self, idx: usize) -> io::Result<()> {
        let mut scratch = [0u8; 256];
        loop {
            match self.replicas[idx].sock.read(&mut scratch) {
                Ok(0) => {
                    self.replicas[idx].close();
                    return Ok(());
                }
                Ok(n) => match &self.replicas[idx].state {
                    ReplicaState::HandshakePending => {
                        let conn = &mut self.replicas[idx];
                        if conn.input.len() + n > HANDSHAKE_MAX_INPUT {
                            conn.close();
                            return Ok(());
                        }
                        conn.input.extend_from_slice(&scratch[..n]);
                        if let Err(e) = advance_handshake(conn) {
                            eprintln!(
                                "kevy: replica handshake rejected on fd {}: {e}",
                                conn.fd,
                            );
                            conn.close();
                            return Ok(());
                        }
                        if !matches!(
                            self.replicas[idx].state,
                            ReplicaState::HandshakePending
                        ) {
                            return Ok(());
                        }
                    }
                    ReplicaState::Streaming { .. } => {
                        let conn = &mut self.replicas[idx];
                        if conn.input.len() + n > STREAMING_INPUT_DISCARD_CAP {
                            eprintln!(
                                "kevy: streaming replica {} sent > {} B \
                                 of unsolicited input; dropping link",
                                conn.fd, STREAMING_INPUT_DISCARD_CAP,
                            );
                            conn.close();
                            return Ok(());
                        }
                        conn.input.clear();
                    }
                    ReplicaState::AckSent { .. }
                    | ReplicaState::SnapshotShipping { .. }
                    | ReplicaState::Closed { .. } => {
                        // No input expected; drain into the void.
                    }
                },
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Drain `output[write_off..]` non-blocking. When fully drained
    /// in `AckSent`, transition to [`ReplicaState::Streaming`]
    /// (carrying the handshake's replica id + from-offset).
    pub(crate) fn replica_writable(&mut self, idx: usize) -> io::Result<()> {
        loop {
            let conn = &mut self.replicas[idx];
            if conn.write_off >= conn.output.len() {
                conn.output.clear();
                conn.write_off = 0;
                if let ReplicaState::AckSent { replica_id, from_offset } = &conn.state {
                    let rid = replica_id.clone();
                    let off = *from_offset;
                    conn.state = ReplicaState::Streaming {
                        replica_id: rid,
                        sent_offset: off,
                    };
                }
                return Ok(());
            }
            match conn.sock.write(&conn.output[conn.write_off..]) {
                Ok(0) => {
                    conn.close();
                    return Ok(());
                }
                Ok(n) => {
                    conn.write_off += n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Remove every replica in [`ReplicaState::Closed`]. Conns whose
    /// `Closed.replica_id` is `Some` are recorded into
    /// [`Shard::slots`] (per T1.15) before dropping so a reconnect
    /// within the window stays correlatable.
    pub(crate) fn reap_closed_replicas(&mut self) {
        // Fast path: no Closed conns. Avoids the Instant::now() cost
        // on every reactor iteration.
        if !self.replicas.iter().any(|r| matches!(r.state, ReplicaState::Closed { .. })) {
            return;
        }
        let now_ns = std::time::Instant::now()
            .duration_since(self.replication_epoch)
            .as_nanos() as u64;
        let mut i = self.replicas.len();
        while i > 0 {
            i -= 1;
            if let ReplicaState::Closed { replica_id, sent_offset } = &self.replicas[i].state {
                if let Some(id) = replica_id.as_ref() {
                    self.slots.insert_or_touch(id, *sent_offset, now_ns);
                }
                let conn = self.replicas.swap_remove(i);
                let _ = self.poller.delete(conn.fd);
                // sock drops here → fd closed.
            }
        }
    }

    /// Periodic expiry of stale slots. Called from the shard tick.
    pub(crate) fn tick_replication_slots(&mut self, now: std::time::Instant) {
        if self.replicate.is_none() || self.slots.is_empty() {
            return;
        }
        let now_ns = now
            .duration_since(self.replication_epoch)
            .as_nanos() as u64;
        let window_ns = u64::from(self.replication_reconnect_window_ms) * 1_000_000;
        let dropped = self.slots.expire(now_ns, window_ns);
        if !dropped.is_empty() {
            eprintln!(
                "kevy: shard {} expired {} replication slot(s) past reconnect window",
                self.id,
                dropped.len(),
            );
        }
    }
}

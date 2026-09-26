//! Per-shard reactor glue for the replication subsystem: accept new
//! replica connections, drive their read/write handlers, reap closed
//! conns into the slot table, and periodically expire stale slots.
//!
//! Split from [`crate::replication`] (which holds the state machine
//! types + `close()` + handshake parser) so each file stays under the
//! 500-LOC house rule. All methods here are `impl<C: Commands> Shard<C>`.

// Wakes and poller edits are advisory: a wake that does not land
// delays the work to the next natural wakeup, and deleting an fd
// the poller has already dropped reports what was wanted. Socket
// options shape latency, not correctness.
#![expect(clippy::let_underscore_must_use, reason = "a missed wake costs a tick, not a result")]

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
/// being dropped. Streaming input is the replica→primary ACK channel
/// (`REPLCONF ACK` lines parsed by `parse_replica_acks`); anything
/// unparseable is skipped, so a well-behaved peer never accumulates
/// much. The cap protects against a peer dumping arbitrary bytes
/// hoping to bloat memory.
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
                    // Capture the replica's peer addr at
                    // accept time so `INFO replication` / `ROLE` can
                    // report it. `peer_addr` errs on a peer that
                    // already vanished — fall back to 0.0.0.0:0,
                    // the connection will reap on the next read.
                    let peer = sock.peer_addr().unwrap_or((std::net::Ipv4Addr::UNSPECIFIED, 0));
                    let mut conn = ReplicaConn::with_peer(sock, peer);
                    if self.repl_security.is_some() {
                        conn.noise = Some(crate::replication_secure::ReplNoise::pending());
                    }
                    self.replicas.push(conn);
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
    /// `close()` on failure. In `Streaming`, parse `REPLCONF ACK`
    /// lines (the replica→primary ACK channel). In `AckSent`/`Closed`,
    /// ignore.
    // LOC-WAIVER: replica-conn read state machine — one read loop over
    // ReplicaState arms (handshake / ACK stream / drain); the loop and
    // state arms are one indivisible protocol unit.
    pub(crate) fn replica_readable(&mut self, idx: usize) -> io::Result<()> {
        let mut scratch = [0u8; 256];
        loop {
            match self.replicas[idx].sock.read(&mut scratch) {
                Ok(0) => {
                    self.replicas[idx].close();
                    return Ok(());
                }
                Ok(n) => {
                    let Some(plain) = self.replica_plaintext(idx, &scratch[..n]) else {
                        return Ok(());
                    };
                    if !self.replica_consume(idx, &plain) {
                        return Ok(());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// The plaintext carried by `raw`: itself on a plain link, whatever the
    /// Noise layer decrypts on a secure one. `None` when the link was closed
    /// for failing the handshake or authentication.
    fn replica_plaintext(&mut self, idx: usize, raw: &[u8]) -> Option<Vec<u8>> {
        let (Some(sec), Some(noise)) =
            (self.repl_security.as_ref(), self.replicas[idx].noise.as_mut())
        else {
            return Some(raw.to_vec());
        };
        let mut plain = Vec::new();
        if let Err(e) = noise.on_bytes(sec, raw, &mut plain) {
            eprintln!("kevy: replica link on fd {} refused: {e}", self.replicas[idx].fd);
            self.replicas[idx].close();
            return None;
        }
        Some(plain)
    }

    /// Feed plaintext to the connection's state machine. `false` when the
    /// read loop should stop (the connection closed, or the handshake just
    /// completed and its reply must be written first).
    fn replica_consume(&mut self, idx: usize, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        match &self.replicas[idx].state {
            ReplicaState::HandshakePending => {
                let conn = &mut self.replicas[idx];
                if conn.input.len() + bytes.len() > HANDSHAKE_MAX_INPUT {
                    conn.close();
                    return false;
                }
                conn.input.extend_from_slice(bytes);
                let feed_gen = self.replicate.as_ref().map_or(0, |f| f.generation());
                let conn = &mut self.replicas[idx];
                if let Err(e) = advance_handshake(conn, feed_gen) {
                    eprintln!("kevy: replica handshake rejected on fd {}: {e}", conn.fd,);
                    conn.close();
                    return false;
                }
                if !matches!(self.replicas[idx].state, ReplicaState::HandshakePending) {
                    if crate::repl_trace() {
                        self.trace_handshake(idx);
                    }
                    return false;
                }
                true
            }
            ReplicaState::Streaming { .. } => {
                let conn = &mut self.replicas[idx];
                if conn.input.len() + bytes.len() > STREAMING_INPUT_DISCARD_CAP {
                    eprintln!(
                        "kevy: streaming replica {} sent > {} B \
                         of unparseable input; dropping link",
                        conn.fd, STREAMING_INPUT_DISCARD_CAP,
                    );
                    conn.close();
                    return false;
                }
                conn.input.extend_from_slice(bytes);
                self.parse_replica_acks(idx);
                true
            }
            ReplicaState::AckSent { .. }
            | ReplicaState::SnapshotShipping { .. }
            | ReplicaState::Closed { .. } => true,
        }
    }

    pub(crate) fn replica_writable(&mut self, idx: usize) -> io::Result<()> {
        if self.replicas[idx].noise.is_some() {
            return replica_writable_sealed(&mut self.replicas[idx]);
        }
        loop {
            let conn = &mut self.replicas[idx];
            if conn.write_off >= conn.output.len() {
                conn.drained();
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

    /// An I/O error on a replica link kills THAT LINK, never the shard.
    ///
    /// A replica that goes away mid-stream makes the primary's next write
    /// to it fail with `EPIPE` / `ECONNRESET`. Propagating that out of the
    /// reactor ends the shard — and with it every client connection the
    /// shard owns, none of which had anything to do with replication.
    /// That is what `kevy: shard N exited with error: Broken pipe`
    /// meant: killing a replica took down the primary's shards, and the
    /// clients saw their connections close with no reply in flight.
    ///
    /// Closing the link is also what the streaming-output cap already
    /// does for the same situation reached a different way; a replica
    /// reconnects and resumes from the backlog.
    pub(crate) fn replica_io_failed(&mut self, idx: usize, what: &str, e: &io::Error) {
        if let Some(conn) = self.replicas.get_mut(idx) {
            eprintln!(
                "kevy: shard {} replica fd {} {what} failed: {e} — dropping link \
                 (reconnect resumes from the backlog)",
                self.id, conn.fd,
            );
            conn.close();
        }
    }

    /// Remove every replica in [`ReplicaState::Closed`]. Conns whose
    /// `Closed.replica_id` is `Some` get their slot's `last_seen_ns`
    /// touched before dropping so a reconnect within the window stays
    /// correlatable — WITHOUT advancing `acked_offset` (the conn
    /// knows what it sent, not what the peer confirmed; a sent-as-
    /// acked write here would let `WAIT` count unconfirmed bytes
    /// through the reconnect window).
    pub(crate) fn reap_closed_replicas(&mut self) {
        // Fast path: no Closed conns. Avoids the Instant::now() cost
        // on every reactor iteration.
        if !self.replicas.iter().any(|r| matches!(r.state, ReplicaState::Closed { .. })) {
            return;
        }
        let now_ns =
            std::time::Instant::now().duration_since(self.replication_epoch).as_nanos() as u64;
        let mut i = self.replicas.len();
        while i > 0 {
            i -= 1;
            if let ReplicaState::Closed { replica_id } = &self.replicas[i].state {
                if let Some(id) = replica_id.as_ref() {
                    self.slots.touch_or_insert_unacked(id, now_ns);
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
        let now_ns = now.duration_since(self.replication_epoch).as_nanos() as u64;
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

/// Seal whatever plaintext is pending, then write sealed bytes until the
/// socket would block.
fn replica_writable_sealed(conn: &mut ReplicaConn) -> io::Result<()> {
    let Some(noise) = conn.noise.as_mut() else { return Ok(()) };
    if let Err(e) = noise.seal(&conn.output[conn.write_off..]) {
        eprintln!("kevy: replica link on fd {} failed to seal: {e}", conn.fd);
        conn.close();
        return Ok(());
    }
    conn.write_off = conn.output.len();
    loop {
        let Some(noise) = conn.noise.as_mut() else { return Ok(()) };
        let wire = noise.wire();
        if wire.is_empty() {
            conn.drained();
            return Ok(());
        }
        match conn.sock.write(wire) {
            Ok(0) => {
                conn.close();
                return Ok(());
            }
            Ok(n) => noise.wrote(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

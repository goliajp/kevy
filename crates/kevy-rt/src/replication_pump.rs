//! Per-iter producer pump that drains the per-shard
//! [`kevy_replicate::source::ReplicationSource`] backlog into the
//! output buffers of streaming replicas. Split out of
//! [`crate::replication`] (which holds the accept + handshake state
//! machine) so each file stays under the 500-LOC house rule.
//!
//! Called from [`crate::shard::Shard::run`] once per reactor iteration.
//! Cost when replication is off = one `Option::is_none()` check; cost
//! with no streaming replicas = one extra `Vec::is_empty()` after.

use crate::Commands;
use crate::replication::ReplicaState;
use crate::shard::Shard;
use kevy_replicate::wire::{
    SNAPSHOT_CHUNK_MAX, encode_snapshot_begin, encode_snapshot_chunk, encode_snapshot_end,
};
use std::io;

/// Per-iter per-replica byte budget. A streaming replica picks up at
/// most this many bytes of new frames per reactor iteration; the
/// remainder waits for the next pump. Prevents a single replica from
/// monopolising a shard's loop time when a large backlog drains.
/// 256 KiB ≈ ~1500 small SET frames per iter.
const PUMP_BYTE_BUDGET_PER_ITER: usize = 256 * 1024;

/// Hard cap on a streaming replica's outbound buffer (bytes appended
/// but not yet written to the socket). Reached when the replica's TCP
/// receive window is full + the primary keeps pushing. v1.18.0 policy
/// on hitting the cap: close the link. A reconnect (within the
/// reconnect window — T1.15 wiring) resumes from the source backlog
/// or full-snapshots (T1.22+). The alternatives (block, retry, or
/// silently drop frames) all corrupt the "every committed write
/// reaches every replica" invariant; closing surfaces the problem.
const STREAMING_OUTPUT_CAP: usize = 4 * 1024 * 1024;

impl<C: Commands> Shard<C> {
    /// Per-iter producer pump. Three phases:
    ///
    /// 1. Walk every replica and dispatch by state — Streaming fills
    ///    from backlog ([`Self::fill_streaming_output`]); SnapshotShipping
    ///    chunks from the in-memory snapshot buffer
    ///    ([`Self::pump_snapshot_chunks`]).
    /// 2. [`Self::drain_streaming_outputs`] tries to write each
    ///    replica's pending output non-blocking; partial writes wait
    ///    on the next writability event.
    pub(crate) fn pump_replication(&mut self) -> io::Result<()> {
        let Some(src) = self.replicate.as_ref() else {
            return Ok(());
        };
        if self.replicas.is_empty() {
            return Ok(());
        }
        let next = src.next_offset();
        for idx in 0..self.replicas.len() {
            match self.replicas[idx].state {
                ReplicaState::Streaming { .. } => self.fill_streaming_output(idx, next),
                ReplicaState::SnapshotShipping { .. } => self.pump_snapshot_chunks(idx),
                _ => {}
            }
        }
        self.drain_streaming_outputs()
    }

    /// Refill one replica's output buffer with backlog frames. Skips
    /// when the conn is not in Streaming, is already caught up, or
    /// has too much pending output (backpressure). Closes the conn on
    /// `TooOld` (need snapshot ship — T1.22+) or `Future` (corrupt
    /// state from a bad peer).
    fn fill_streaming_output(&mut self, idx: usize, primary_next: u64) {
        let ReplicaState::Streaming { sent_offset, .. } = self.replicas[idx].state else {
            return;
        };
        if sent_offset >= primary_next {
            return; // caught up
        }
        let pending = self.replicas[idx].output.len() - self.replicas[idx].write_off;
        if pending >= STREAMING_OUTPUT_CAP / 2 {
            return; // backpressure — let the socket drain first
        }
        let Some(src) = self.replicate.as_ref() else {
            return;
        };
        let frames = match src.frames_from(sent_offset) {
            Ok(it) => it,
            Err(kevy_replicate::source::FromOffset::TooOld) => {
                // Replica fell behind the backlog window. Trigger a
                // snapshot ship (T1.23): serialize the local store
                // in-memory now, transition the conn to
                // SnapshotShipping, the next pump iteration chunks
                // it out via pump_snapshot_chunks.
                if let Err(e) = self.start_snapshot_ship(idx, primary_next) {
                    eprintln!(
                        "kevy: replica fd {} snapshot ship trigger failed: {e}; dropping link",
                        self.replicas[idx].fd,
                    );
                    self.replicas[idx].close();
                }
                return;
            }
            Err(kevy_replicate::source::FromOffset::Future) => {
                eprintln!(
                    "kevy: replica fd {} sent_offset {} > primary next {}; \
                     corrupt state, dropping link",
                    self.replicas[idx].fd, sent_offset, primary_next,
                );
                self.replicas[idx].close();
                return;
            }
        };
        // Copy frame bytes into a local Vec first so the mutable
        // borrow of `self.replicas[idx].output` doesn't overlap with
        // the immutable borrow of `src` via `frames`.
        let mut append = Vec::new();
        let mut new_sent = sent_offset;
        let mut bytes_this_pump = 0usize;
        for frame in frames {
            if bytes_this_pump + frame.bytes.len() > PUMP_BYTE_BUDGET_PER_ITER
                || pending + bytes_this_pump + frame.bytes.len() > STREAMING_OUTPUT_CAP
            {
                break;
            }
            append.extend_from_slice(&frame.bytes);
            bytes_this_pump += frame.bytes.len();
            new_sent = frame.offset + 1;
        }
        if !append.is_empty() {
            let conn = &mut self.replicas[idx];
            conn.output.extend_from_slice(&append);
            if let ReplicaState::Streaming { sent_offset, .. } = &mut conn.state {
                *sent_offset = new_sent;
            }
        }
    }

    /// Drain every streaming / ack-pending / snapshot-shipping
    /// replica's output buffer non-blocking. Partial writes wait for
    /// the next writability event. Replicas whose output stays at
    /// the cap after a drain attempt are closed.
    fn drain_streaming_outputs(&mut self) -> io::Result<()> {
        for idx in 0..self.replicas.len() {
            if !matches!(
                self.replicas[idx].state,
                ReplicaState::Streaming { .. }
                    | ReplicaState::AckSent { .. }
                    | ReplicaState::SnapshotShipping { .. }
            ) {
                continue;
            }
            if self.replicas[idx].output.len() <= self.replicas[idx].write_off {
                continue;
            }
            self.replica_writable(idx)?;
            let conn = &self.replicas[idx];
            if matches!(conn.state, ReplicaState::Streaming { .. })
                && conn.output.len() - conn.write_off >= STREAMING_OUTPUT_CAP
            {
                eprintln!(
                    "kevy: streaming replica fd {} output cap ({} B) reached; \
                     dropping link (reconnect will resume from backlog)",
                    conn.fd, STREAMING_OUTPUT_CAP,
                );
                self.replicas[idx].close();
            }
        }
        Ok(())
    }

    /// Trigger a snapshot ship (T1.23) for the replica at `idx`:
    /// in-memory-serialize the local store via `kevy_persist::
    /// write_snapshot_to`, push `+SNAPSHOT\r\n` to the conn's
    /// output, and transition state to SnapshotShipping. The next
    /// pump iteration chunks the buffer out via
    /// [`Self::pump_snapshot_chunks`].
    ///
    /// `ack_offset` is the primary's `source.next_offset()` at
    /// trigger time — encoded into `+SNAPSHOT_END <ack_offset>\r\n`
    /// when the snapshot ship completes, and becomes the replica's
    /// new `sent_offset` for live streaming after.
    ///
    /// T1.23.5: freeze a COW [`kevy_store::SnapshotView`] on the
    /// reactor thread (O(n) shallow clone — ns/entry, much cheaper
    /// than full serialization), hand it to a background worker
    /// that runs `kevy_persist::write_snapshot_to` at leisure, and
    /// emit `+SNAPSHOT\r\n` immediately so the replica knows the
    /// ship started. The worker `mpsc::send`s the serialized bytes
    /// back when done; [`Self::pump_snapshot_chunks`] polls each
    /// tick via `try_recv` and starts emitting chunks once they
    /// arrive. The reactor no longer pauses for the duration of
    /// serialization — only for the shallow collect.
    fn start_snapshot_ship(&mut self, idx: usize, ack_offset: u64) -> io::Result<()> {
        let ReplicaState::Streaming { ref replica_id, .. } = self.replicas[idx].state else {
            // Defensive: only Streaming replicas should reach the
            // TooOld branch in fill_streaming_output.
            return Ok(());
        };
        let replica_id = replica_id.clone();
        let view = self.store.collect_snapshot();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name(format!("kevy-snapshot-{replica_id}"))
            .spawn(move || {
                let mut buf = Vec::new();
                if kevy_persist::write_snapshot_to(&view, &mut buf).is_ok() {
                    let _ = tx.send(buf);
                }
                // On serialization error, drop tx → receiver-side
                // try_recv returns Disconnected; pump_snapshot_chunks
                // treats that as a fatal error and closes the conn.
            })
            .expect("spawn snapshot serializer thread");
        let conn = &mut self.replicas[idx];
        conn.output.extend_from_slice(&encode_snapshot_begin());
        conn.state = ReplicaState::SnapshotShipping {
            replica_id,
            ack_offset,
            serializing: Some(rx),
            snapshot_buf: Vec::new(),
            snapshot_off: 0,
        };
        Ok(())
    }

    /// Chunk one SNAPSHOT_CHUNK_MAX worth of snapshot bytes into the
    /// replica's output. When the buffer is fully sent, pushes the
    /// `+SNAPSHOT_END <ack_offset>\r\n` trailer and transitions to
    /// Streaming. Skips when pending output is over half the cap
    /// (backpressure — drain_streaming_outputs will write what's
    /// queued; next pump retries).
    ///
    /// T1.23.5: if the background serializer hasn't delivered yet,
    /// `try_recv` returns `Empty`; the pump no-ops this iteration
    /// and retries next tick. If the worker thread died without
    /// sending (`Disconnected`), the conn is closed.
    fn pump_snapshot_chunks(&mut self, idx: usize) {
        let pending = self.replicas[idx].output.len() - self.replicas[idx].write_off;
        if pending >= STREAMING_OUTPUT_CAP / 2 {
            return;
        }
        // Try to receive the serialized bytes if the worker is still
        // running. Three outcomes: bytes arrived → populate
        // snapshot_buf; channel empty → wait; channel closed without
        // bytes → fatal, close the conn.
        if let ReplicaState::SnapshotShipping {
            ref mut serializing,
            ref mut snapshot_buf,
            ..
        } = self.replicas[idx].state
            && let Some(rx) = serializing.take()
        {
            match rx.try_recv() {
                Ok(buf) => {
                    *snapshot_buf = buf;
                    // Drop the receiver — already consumed.
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Worker still running; put the receiver
                    // back and try next tick.
                    *serializing = Some(rx);
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    eprintln!(
                        "kevy: snapshot serializer thread died for replica fd {} — closing",
                        self.replicas[idx].fd,
                    );
                    self.replicas[idx].close();
                    return;
                }
            }
        }
        let (ack_offset, chunk_bytes, done) = {
            let ReplicaState::SnapshotShipping {
                ref snapshot_buf,
                snapshot_off,
                ack_offset,
                ..
            } = self.replicas[idx].state
            else {
                return;
            };
            let remaining = &snapshot_buf[snapshot_off..];
            if remaining.is_empty() {
                (ack_offset, Vec::new(), true)
            } else {
                let take = remaining.len().min(SNAPSHOT_CHUNK_MAX);
                (ack_offset, remaining[..take].to_vec(), false)
            }
        };
        let conn = &mut self.replicas[idx];
        if !chunk_bytes.is_empty() {
            conn.output.extend_from_slice(&encode_snapshot_chunk(&chunk_bytes));
            if let ReplicaState::SnapshotShipping {
                ref mut snapshot_off, ..
            } = conn.state
            {
                *snapshot_off += chunk_bytes.len();
            }
        }
        if done {
            // Snapshot fully chunked — emit the end marker and flip
            // state to Streaming so the next pump fills from the
            // backlog at `ack_offset`.
            conn.output.extend_from_slice(&encode_snapshot_end(ack_offset));
            if let ReplicaState::SnapshotShipping { replica_id, .. } = &conn.state {
                let rid = replica_id.clone();
                conn.state = ReplicaState::Streaming {
                    replica_id: rid,
                    sent_offset: ack_offset,
                };
            }
        }
    }
}

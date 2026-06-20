//! io_uring per-connection / park state — the byte buffers and flags whose
//! addresses in-flight SQEs point at. Split from [`crate::uring_reactor`]
//! to keep that file under the 500-LOC house rule.

use kevy_uring::{Iovec, KernelTimespec};
use std::sync::Arc;

/// io_uring-specific per-connection state (the byte buffers that must outlive
/// their in-flight SQEs). The command-level state stays in the shard's [`Conn`].
pub(crate) struct UringConn {
    // Fields are pub(crate) for the reap loop in [`crate::uring_inbox`].
    /// A multishot recv SQE is armed for this conn (re-fires per arrival, drawing
    /// from the shard's provided-buffer ring). Re-armed only when it terminates.
    pub(crate) recv_armed: bool,
    /// Stable buffer for an in-flight write (swapped in from `Conn::output`).
    pub(crate) write_buf: Vec<u8>,
    pub(crate) write_off: usize,
    pub(crate) write_inflight: bool,
    /// L1 (2026-06-21): Arc-backed value bytes pinned for the in-flight
    /// `writev`. Each `(pos, arc)` means "insert `arc.as_ref()` after byte
    /// `pos` in `write_buf` when building the iovec list". Sorted by `pos`
    /// (encode pushes in order so they're naturally sorted). The Arcs keep
    /// the bytes alive across the SQE→CQE window even if the keyspace
    /// mutates. Empty in the steady-state small-reply path → reactor stays
    /// on `prep_write` (no overhead).
    pub(crate) write_arcs: Vec<(usize, Arc<[u8]>)>,
    /// Reusable iovec scratch for `prep_writev` — sized to hold the iovecs
    /// for one writev submission. Lives in `UringConn` rather than on the
    /// stack so the kernel's async iovec read sees a stable address until
    /// the matching CQE fires.
    pub(crate) write_iovecs: Vec<Iovec>,
    /// EOF/error seen on the socket — close once writes drain.
    pub(crate) closing: bool,
}

impl UringConn {
    pub(crate) fn new() -> Self {
        UringConn {
            recv_armed: false,
            write_buf: Vec::new(),
            write_off: 0,
            write_inflight: false,
            write_arcs: Vec::new(),
            write_iovecs: Vec::new(),
            closing: false,
        }
    }
}

/// Parked-wait state: the waker-pipe read buffer and timeout payload that
/// in-flight park SQEs point at. Lives on `run_uring`'s stack for the
/// reactor's whole life, so the kernel-side pointers stay valid across
/// iterations (a wake may reap only one of the two CQEs; the other SQE
/// stays in flight into later parks).
#[derive(Default)]
pub(crate) struct ParkState {
    /// A read SQE on the waker pipe is in flight.
    pub(crate) waker_armed: bool,
    /// A timeout SQE is in flight (bounds the blocking wait; a leftover
    /// one from an earlier park just shortens the next park — harmless).
    pub(crate) timeout_inflight: bool,
    pub(crate) wake_buf: [u8; 8],
    pub(crate) ts: KernelTimespec,
}

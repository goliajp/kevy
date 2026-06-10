//! io_uring per-connection / park state — the byte buffers and flags whose
//! addresses in-flight SQEs point at. Split from [`crate::uring_reactor`]
//! to keep that file under the 500-LOC house rule.

use kevy_uring::KernelTimespec;

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

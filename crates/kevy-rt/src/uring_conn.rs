//! io_uring per-connection / park state — the byte buffers and flags whose
//! addresses in-flight SQEs point at. Split from [`crate::uring_reactor`]
//! to keep that file under the 500-LOC house rule.

use kevy_uring::{Iovec, KernelTimespec};
use std::sync::Arc;
use std::time::Duration;

/// **v1.25 B.4 + A.2 (post-2026-06-22)** — per-conn state for the BigBulk
/// SET ingest path. When the parser sees `SET key $<N>\r\n` with N ≥
/// [`crate::uring_io::BIG_ARG_PROMOTE_THRESHOLD`] and the value body is not
/// yet complete in the current recv chunk, the reactor:
///
/// 1. Pre-parses verb (SET only for now), key, and SET options off the
///    header bytes (which already arrived).
/// 2. Allocates `buf = Vec::with_capacity(N + 2)` — exactly the body + CRLF.
/// 3. Copies any body bytes already received (slab tail past the header)
///    into `buf`.
/// 4. Routes every subsequent multishot-recv CQE on this conn into `buf`
///    instead of the conn's `input` Vec, until `buf.len() == N + 2`.
/// 5. Builds the value via `pick_value_for_set_owned(buf)` — `Vec` →
///    `Box<[u8]>` (cap == len after `into_boxed_slice`) → `Arc::from(Box)`
///    is **zero-copy**: the existing heap allocation becomes the Arc body.
/// 6. Calls `Store::set(...)` to apply the SET, writes `+OK\r\n` to the
///    conn's output, and clears `pending_big_arg`. Multishot recv stays
///    armed throughout — only the routing of CQE bytes changes.
///
/// Eliminates both:
/// - The realloc storm in `conn.input` (0→16→32→48→64 K for a 64 K SET);
/// - The final `Arc::from(&[u8])` 64 K alloc + memcpy (which currently
///   doubles the per-byte work for any value > slab size).
///
/// Scope: SET only. Other multi-arg-with-value commands (MSET, SETEX,
/// APPEND, GETSET, …) keep the current borrowed-slice path.
pub(crate) struct BigArgState {
    /// Owned destination for the value bulk body **only** — capacity is
    /// exactly `body_len` so `Vec::into_boxed_slice()` is genuinely
    /// zero-copy at completion (no `realloc(cap → len)` shrink). The
    /// trailing CRLF is consumed separately via [`Self::crlf_needed`].
    pub(crate) buf: Vec<u8>,
    /// Length of the value bulk (in bytes), i.e. `N` from `$<N>\r\n`.
    pub(crate) body_len: usize,
    /// Bytes of trailing `\r\n` still to consume off the wire AFTER the
    /// body completes. Starts at 2; decrements as the kernel delivers
    /// the trailer. The body is "done" when `buf.len() == body_len AND
    /// crlf_needed == 0`.
    pub(crate) crlf_needed: u8,
    /// SET key (owned copy; the slab buffer that originally carried the
    /// header bytes gets recycled before we re-enter `uring_on_recv` for
    /// the body, so we can't keep a borrow).
    pub(crate) key: Box<[u8]>,
    /// SET option set parsed from the header.
    pub(crate) opts: BigArgSetOptions,
}

/// SET-command options pre-parsed from the header (mirror of the option
/// loop in `kevy/src/cmd_data.rs::cmd_set`). Defaults to no-option for the
/// bench shape `SET key value`.
#[derive(Default, Clone, Copy)]
pub(crate) struct BigArgSetOptions {
    pub(crate) expire: Option<Duration>,
    pub(crate) nx: bool,
    pub(crate) xx: bool,
    /// Set if header parsing rejects the options (syntax error / invalid
    /// expire / NX+XX). The body still has to be drained off the wire so
    /// the next frame stays aligned; on completion we emit the RESP error
    /// rather than a SET.
    pub(crate) syntax_error: bool,
}

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
    /// **v1.25 B.4 + A.2** — when `Some`, the multishot recv handler
    /// routes every byte of the next CQE batch(es) into the owned
    /// `BigArgState::buf` instead of the conn's `input` Vec. Cleared on
    /// completion (full body + CRLF received) or on connection close.
    /// See [`BigArgState`] for the full state machine.
    pub(crate) pending_big_arg: Option<Box<BigArgState>>,
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
            pending_big_arg: None,
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

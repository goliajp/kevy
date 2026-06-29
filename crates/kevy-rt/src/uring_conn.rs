//! io_uring per-connection / park state — the byte buffers and flags whose
//! addresses in-flight SQEs point at. Split from [`crate::uring_reactor`]
//! to keep that file under the 500-LOC house rule.

use kevy_uring::{Iovec, KernelTimespec};
use std::sync::Arc;

/// **v1.25 B.4 + A.2 / B.5 (post-2026-06-22)** — per-conn state for the
/// BigBulk frame-stitch ingest path.
///
/// When the parser sees a `*<argc> <supported-verb> … $N` frame whose
/// LAST bulk has `N ≥ BIG_ARG_PROMOTE_THRESHOLD` and whose body isn't
/// fully present in the current recv chunk, the reactor:
///
/// 1. Walks the frame header to compute the total RESP frame length
///    (header + every bulk's body + every CRLF).
/// 2. Allocates `frame = Vec::with_capacity(total)` — exactly the
///    expected frame size so subsequent `extend_from_slice` calls never
///    reallocate (no 0→16→32→48→64K realloc storm in `conn.input`).
/// 3. Copies all already-received bytes (slab head past the parsed
///    prefix) into `frame`.
/// 4. Routes every subsequent multishot-recv CQE on this conn into
///    `frame` until `frame.len() == total`.
/// 5. Re-dispatches the assembled frame through the normal parser
///    (`Shard::dispatch_batch`). Every existing command handler (SET,
///    SETEX, PSETEX, APPEND, GETSET, MSET, …) runs unchanged — same
///    routing, same AOF, same reply emission.
///
/// Eliminates the conn.input realloc storm. The final `Arc::from(&[u8])`
/// memcpy at SET adoption remains (the handlers take borrowed slices)
/// — that's a v1.25.x lever once frame stitching is proven. The
/// originally-shipped B.4 bare-SET zero-copy adoption was retired
/// because it bypassed cross-shard routing (`self.store.set` writes
/// directly to the connection's owning shard rather than the key's
/// owning shard — a silent data-loss bug on multi-shard setups when
/// the key hashes off-shard).
///
/// Variants supported (last bulk must be the big one):
/// - `SET key <BIG>` (plain 3-arg)
/// - `SETEX key ttl <BIG>`
/// - `PSETEX key ms <BIG>`
/// - `APPEND key <BIG>`
/// - `GETSET key <BIG>`
/// - `MSET k1 v1 … kn <BIG>` (only when LAST value is big)
///
/// Out of scope (v1.25.x follow-up): `SET k <BIG> EX 10` (big value not
/// last); `MSET k1 <BIG> k2 v2` (big value not last). These keep the
/// borrowed-slice path.
pub(crate) enum BigArgState {
    /// **v1.25 B.5 path** — SETEX / PSETEX / APPEND / GETSET / MSET,
    /// OR cross-shard bare-SET, OR (defensive) bare-SET probe that
    /// failed shard-affinity at promote time. Frame Vec accumulates
    /// the whole RESP message via slab→memcpy; on completion runs
    /// through `dispatch_batch`. v1.28 byte-identical behavior.
    Frame {
        /// Capacity equals `total`; subsequent `extend_from_slice`
        /// never reallocates.
        frame: Vec<u8>,
        /// Total expected RESP frame length. Frame complete when
        /// `frame.len() == total`.
        total: usize,
    },
    /// **v1.29 B2-alt** — local-shard bare-SET, mid-cancel of the
    /// multishot recv. Both flag fields start `false`; the kernel
    /// emits two CQEs in either order:
    /// - `OP_BIG_CANCEL` CQE: handler sets `cancel_acked = true`.
    /// - Terminal `OP_RECV` CQE with `res = -ECANCELED`: handler in
    ///   `uring_on_recv` sets `target_canceled = true`.
    /// When BOTH flip, the state transitions to [`Self::BareSetReading`]
    /// and a single-shot `prep_read` SQE is submitted directly into
    /// `body` for the remaining bytes.
    ///
    /// In-flight multishot CQEs carrying actual data may also arrive
    /// during this phase (after cancel was queued but before kernel
    /// processed it). They land in `uring_on_recv`'s normal path and
    /// `extend_from_slice` into `body` as in v1.25 B.5 — same slab→
    /// body memcpy cost as v1.28 for those bytes. The B2-alt win is on
    /// the bytes that arrive AFTER ECANCELED, via the single-shot read.
    BareSetCancelling {
        /// Pre-extracted SET key (small alloc at promote).
        key: Vec<u8>,
        /// Body Vec. **Capacity is fixed at `body_len` EXACTLY** so
        /// `Vec::into_boxed_slice` (called inside
        /// `pick_value_for_set_owned`'s `Arc::new(bytes.into_boxed_slice())`)
        /// is a zero-copy allocation reuse — the v1.29 Option A win
        /// hinges on `len == capacity` at hand-off (else shrink_to_fit
        /// triggers a realloc + memcpy). Trailing CRLF is tracked in
        /// `crlf_seen` and never enters this Vec.
        body: Vec<u8>,
        /// Target value length (the N from `$<N>\r\n`).
        body_len: usize,
        /// Count of trailing CRLF bytes consumed from the wire. `0` at
        /// promote, `2` when the trailing `\r\n` has been seen. Body
        /// Vec stays at `len == capacity == body_len`.
        crlf_seen: u8,
        /// `OP_BIG_CANCEL` CQE seen yet.
        cancel_acked: bool,
        /// Terminal `OP_RECV` CQE seen with `res = -ECANCELED`.
        target_canceled: bool,
    },
    /// **v1.29 B2-alt** — single-shot `prep_read` is in flight; kernel
    /// writes recv bytes directly into `body` (no userspace memcpy).
    /// On `OP_BIG_READ` CQE: advance `body.set_len(body.len() + res)`;
    /// if `body.len() < body.capacity()`, re-submit another
    /// `prep_read` for the remaining bytes; if `body.len() ==
    /// body.capacity()`, finalize via the local-shard fast path and
    /// re-arm the multishot for pipelined commands.
    BareSetReading {
        key: Vec<u8>,
        /// Capacity = `body_len` exactly (same invariant as
        /// `BareSetCancelling`).
        body: Vec<u8>,
        body_len: usize,
        /// CRLF bytes already consumed (carried over from
        /// `BareSetCancelling` at transition). Body Vec is complete
        /// when `body.len() == body_len && crlf_seen == 2`.
        crlf_seen: u8,
    },
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
    pub(crate) write_arcs: Vec<(usize, Arc<Box<[u8]>>)>,
    /// Reusable iovec scratch for `prep_writev` — sized to hold the iovecs
    /// for one writev submission. Lives in `UringConn` rather than on the
    /// stack so the kernel's async iovec read sees a stable address until
    /// the matching CQE fires.
    pub(crate) write_iovecs: Vec<Iovec>,
    /// **A.4 (v1.25)**: how many leading entries of `write_arcs` are
    /// covered by the currently in-flight `writev` SQE. A pipelined
    /// pub/sub flood (`BATCH = 1024` publishes × 50 subs) accumulates
    /// thousands of arcs per conn; one writev is capped by Linux
    /// `IOV_MAX = 1024`, so a single SQE can only cover a prefix. The
    /// reactor submits one chunk per arm_conns iter and drops the
    /// processed prefix on CQE. Zero in the small-output / non-arc
    /// path.
    pub(crate) arcs_in_flight: usize,
    /// **A.4 (v1.25)**: byte position in `write_buf` where the current
    /// in-flight writev submission stops including header bytes (i.e.
    /// the right edge of the last write_buf range packed into the
    /// iovec). On CQE we advance `write_off` to this value. When the
    /// submission covers all arcs and the full tail this equals
    /// `write_buf.len()`. Zero when no chunked writev is in flight.
    pub(crate) write_byte_cap: usize,
    /// **A.4 (v1.25)**: total bytes the kernel was asked to write for
    /// the in-flight writev (sum of all iovec lens). On CQE compared
    /// against `res` to distinguish full vs short writes for the
    /// chunked-writev state machine. Zero when no writev is in flight.
    pub(crate) write_inflight_bytes: usize,
    /// **K4 (v1.25 A.9)**: this conn is already on the shard's
    /// `arm_pending` queue this iter. Dedupes wake-up pushes from the
    /// recv / write / accept / dispatch / publish paths so a single
    /// `arm_conns` visit covers all of them. Cleared in `arm_conns`
    /// right before processing.
    pub(crate) arm_queued: bool,
    /// **v1.29 B2-alt** — the conn needs a cancel SQE for its in-flight
    /// multishot recv on the next [`Shard::uring_arm_conns`] visit (the
    /// big-arg state machine is transitioning to single-shot `prep_read`
    /// for the remaining body bytes). Cleared once the cancel SQE is
    /// queued.
    pub(crate) big_arg_cancel_pending: bool,
    /// **v1.29 B2-alt** — the conn needs a single-shot `prep_read` SQE
    /// on the next [`Shard::uring_arm_conns`] visit. Set when the
    /// cancel/target cancellation pair completes, OR after a partial
    /// `prep_read` CQE leaves body bytes still pending. Cleared once
    /// the SQE is queued.
    pub(crate) big_arg_read_pending: bool,
    /// **v1.29 B2-alt** — the conn needs its multishot recv re-armed
    /// on the next [`Shard::uring_arm_conns`] visit (the big-arg body
    /// is fully received and the conn returns to normal recv mode).
    /// Cleared once the recv SQE is queued.
    pub(crate) big_arg_rearm_recv: bool,
    /// **v1.29 B2-alt** — count of leading bytes to discard from the
    /// next multishot recv slab(s) before resuming normal RESP
    /// dispatch. Set to 2 after the kernel-direct `prep_read` finishes
    /// the body without consuming the trailing `\r\n` (which is still
    /// in the TCP buffer and arrives via the re-armed multishot).
    /// `uring_recv_dispatch` checks this counter and slices the slab
    /// head before parsing.
    pub(crate) pending_crlf_skip: u8,
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
            arcs_in_flight: 0,
            write_byte_cap: 0,
            write_inflight_bytes: 0,
            arm_queued: false,
            big_arg_cancel_pending: false,
            big_arg_read_pending: false,
            big_arg_rearm_recv: false,
            pending_crlf_skip: 0,
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

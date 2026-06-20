//! Per-connection state owned by its origin shard.

use crate::message::PendingSlot;
use kevy_resp::{Argv, RespVersion};
use kevy_sys::Socket;
use std::collections::{HashSet, VecDeque};

/// Per-connection state owned by its origin shard.
///
/// A4 (2026-06-20): `#[repr(C)]` + hot-first field order. The H7 +
/// post-v1.24 diagnostics both showed L1D-miss in the busy-poll
/// reactor body. Per-request work touches `sock` (raw fd for write
/// SQE), `input`/`output` (recv → parse → reply), `pending` (push +
/// pop + front), `write_pos`, `next_seq`, `next_emit`, plus the five
/// boolean / enum flags. The four collection fields (`sub`, `psub`,
/// `multi`, `watched`) are PUBLISH / MULTI / WATCH only — touched
/// zero times on the steady-state GET/SET loop.
///
/// Layout under `#[repr(C)]`:
///
/// ```text
///   offset  bytes  field
///   0       4      sock        (raw fd)
///   8       24     input       (Vec ptr + len + cap)
///   32      24     output      (Vec ptr + len + cap)
///   56      32     pending     (VecDeque<PendingSlot>)
///   88      8      write_pos
///   96      8      next_seq
///   104     8      next_emit
///   112     1      proto       (enum tag)
///   113     1      closing
///   114     1      want_write
///   115     1      blocked
///   116     1      cluster
///   ──── ~120 B = 2 cache lines worth of hot state ────
///   COLD
///   ?       ?      sub, psub, multi, watched
/// ```
///
/// Without `#[repr(C)]`, the default `repr(Rust)` layout reorders for
/// alignment + size and intersperses cold collections among the hot
/// flags. With this attribute the hot fields are guaranteed
/// contiguous at the front of the struct, fitting two cache lines.
#[repr(C)]
pub(crate) struct Conn {
    // ── HOT (~2 cache lines, touched on every recv/dispatch/write iter) ──
    pub(crate) sock: Socket,
    pub(crate) input: Vec<u8>,
    pub(crate) output: Vec<u8>,
    /// Outstanding commands in seq order; front == `next_emit`. An O(1) ring
    /// that replaces the per-command HashMap churn.
    pub(crate) pending: VecDeque<PendingSlot>,
    pub(crate) write_pos: usize,
    /// Next seq to assign (== `next_emit + pending.len()`).
    pub(crate) next_seq: u64,
    /// Seq of `pending.front()` — the next reply to emit.
    pub(crate) next_emit: u64,
    /// Which RESP version this connection speaks. Negotiated via
    /// `HELLO`: a fresh conn defaults to RESP2 (Redis 6.x and earlier);
    /// the conn switches to RESP3 when the client sends `HELLO 3`, at
    /// which point `dispatch_into_resp3` becomes the per-command reply
    /// encoder.
    pub(crate) proto: RespVersion,
    /// QUIT / EOF / protocol error seen — close once drained & flushed.
    pub(crate) closing: bool,
    pub(crate) want_write: bool,
    /// Set while this conn is parked in a `BLPOP` / `BRPOP` /
    /// `XREAD BLOCK` waiter. The dispatcher refuses new command processing
    /// on this conn until a wake (write to a watched key) or a tick-driven
    /// timeout clears the flag.
    pub(crate) blocked: bool,
    /// Accepted on this shard's per-shard cluster listener (vs the shared
    /// SO_REUSEPORT compat port). Cluster conns get `-MOVED` for
    /// wrong-shard single-key commands instead of transparent forwarding.
    pub(crate) cluster: bool,

    // ── COLD (PUBLISH / MULTI / WATCH only; never touched on the
    //    GET/SET steady state) ──
    /// Channels this connection is subscribed to (pub/sub).
    pub(crate) sub: HashSet<Vec<u8>>,
    /// Glob patterns this connection has `PSUBSCRIBE`-d. Disjoint from
    /// `sub` — a PUBLISH that matches both yields one `message` and one
    /// `pmessage` frame (Redis semantics).
    pub(crate) psub: HashSet<Vec<u8>>,
    /// Queued commands inside a MULTI…EXEC transaction (`None` = not in
    /// MULTI).
    pub(crate) multi: Option<Vec<Argv>>,
    /// `WATCH`-ed keys + the version each had on its owning shard at
    /// `WATCH` time. `EXEC` fans these out via `Op::CheckWatch`; if any
    /// shard reports a mismatch, the transaction aborts (nil multi-bulk).
    pub(crate) watched: Vec<(Vec<u8>, u64)>,
}

impl Conn {
    pub(crate) fn new(sock: Socket) -> Self {
        Conn {
            sock,
            input: Vec::new(),
            output: Vec::new(),
            write_pos: 0,
            want_write: false,
            next_seq: 0,
            next_emit: 0,
            closing: false,
            pending: VecDeque::new(),
            sub: HashSet::new(),
            psub: HashSet::new(),
            multi: None,
            watched: Vec::new(),
            proto: RespVersion::default(),
            blocked: false,
            cluster: false,
        }
    }
}

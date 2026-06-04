//! Per-connection state owned by its origin shard.

use crate::message::PendingSlot;
use kevy_resp::{Argv, RespVersion};
use kevy_sys::Socket;
use std::collections::{HashSet, VecDeque};

/// Per-connection state owned by its origin shard.
pub(crate) struct Conn {
    pub(crate) sock: Socket,
    pub(crate) input: Vec<u8>,
    pub(crate) output: Vec<u8>,
    pub(crate) write_pos: usize,
    pub(crate) want_write: bool,
    /// Next seq to assign (== `next_emit + pending.len()`).
    pub(crate) next_seq: u64,
    /// Seq of `pending.front()` — the next reply to emit.
    pub(crate) next_emit: u64,
    /// QUIT / EOF / protocol error seen — close once drained & flushed.
    pub(crate) closing: bool,
    /// Outstanding commands in seq order; front == `next_emit`. An O(1) ring
    /// that replaces the per-command HashMap churn.
    pub(crate) pending: VecDeque<PendingSlot>,
    /// Channels this connection is subscribed to (pub/sub).
    pub(crate) sub: HashSet<Vec<u8>>,
    /// Glob patterns this connection has `PSUBSCRIBE`-d. Disjoint from
    /// `sub` — a PUBLISH that matches both yields one `message` and one
    /// `pmessage` frame (Redis semantics). Empty for the vast majority
    /// of conns (no pattern subscribers), so the steady-state cost is one
    /// `HashSet::is_empty()` check per delivery candidate.
    pub(crate) psub: HashSet<Vec<u8>>,
    /// Queued commands inside a MULTI…EXEC transaction (`None` = not in MULTI).
    pub(crate) multi: Option<Vec<Argv>>,
    /// `WATCH`-ed keys + the version each had on its owning shard at
    /// `WATCH` time. `EXEC` fans these out to every relevant shard via
    /// `Op::CheckWatch`; if any shard reports a mismatch, the
    /// transaction aborts (nil multi-bulk). Cleared on EXEC / DISCARD
    /// / UNWATCH / connection close. Empty in steady state for conns
    /// that never call `WATCH` (most clients).
    pub(crate) watched: Vec<(Vec<u8>, u64)>,
    /// Which RESP version this connection speaks. Negotiated via
    /// `HELLO`: a fresh conn defaults to RESP2 (Redis 6.x and earlier);
    /// the conn switches to RESP3 when the client sends `HELLO 3`, at
    /// which point `dispatch_into_resp3` becomes the per-command
    /// reply encoder. Per-conn so a RESP2 client and a RESP3 client
    /// can share the same server without either paying for the other's
    /// shape.
    pub(crate) proto: RespVersion,
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
        }
    }
}

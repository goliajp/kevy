//! Per-connection state owned by its origin shard.

use crate::message::PendingSlot;
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
    /// Queued commands inside a MULTI…EXEC transaction (`None` = not in MULTI).
    pub(crate) multi: Option<Vec<Vec<Vec<u8>>>>,
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
            multi: None,
        }
    }
}

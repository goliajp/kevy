//! Per-connection state owned by its origin shard.

use crate::message::PendingSlot;
use kevy_resp::Argv;
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
    pub(crate) multi: Option<Vec<Argv>>,
    /// `WATCH`-ed keys + the version each had on its owning shard at
    /// `WATCH` time. `EXEC` fans these out to every relevant shard via
    /// `Op::CheckWatch`; if any shard reports a mismatch, the
    /// transaction aborts (nil multi-bulk). Cleared on EXEC / DISCARD
    /// / UNWATCH / connection close. Empty in steady state for conns
    /// that never call `WATCH` (most clients).
    ///
    /// Foundation only — the EXEC fan-out path that consumes this
    /// field lands in the next commit. `dead_code` until then.
    #[allow(dead_code)]
    pub(crate) watched: Vec<(Vec<u8>, u64)>,
    /// Set while a fan-out `Op::CheckWatch` is in flight for this
    /// conn's pending EXEC. New `WATCH` calls inside an in-flight
    /// EXEC are forbidden by Redis semantics; this flag plus
    /// `multi.is_some()` lets the handler reject them with the right
    /// error string.
    ///
    /// Foundation only — read sites land in the next commit. `dead_code`
    /// until then.
    #[allow(dead_code)]
    pub(crate) exec_checking: bool,
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
            watched: Vec::new(),
            exec_checking: false,
        }
    }
}

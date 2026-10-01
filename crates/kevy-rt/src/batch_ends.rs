//! The two ends of a batch: the reactor parsing a connection's commands
//! and dispatching each, and the owning shard prefetching the keys of a
//! batch forwarded to it.

use kevy_resp::parse_command_borrowed;

use crate::Commands;
use crate::message_kinds::DispatchMeta;
use crate::shard::Shard;

/// How many requests ahead of the one running the owner prefetches: far
/// enough that a bucket arrives from memory before its request runs, near
/// enough that the lines in flight fit the core's miss buffers.
pub(crate) const PREFETCH_AHEAD: usize = 4;

/// What [`Shard::dispatch_batch`] saw: how far the parse cursor got,
/// whether it stopped on malformed input, and whether the conn was
/// closed by one of its own commands (QUIT) mid-batch.
pub(crate) struct BatchOutcome {
    pub(crate) consumed: usize,
    pub(crate) protocol_error: bool,
    pub(crate) conn_gone: bool,
}

impl<C: Commands> Shard<C> {
    /// Parse and dispatch every complete RESP command at the front of
    /// `buf` (the borrowed-argv hot path shared by both reactors). The
    /// caller owns buffer bookkeeping (tail retention) and the AOF
    /// group-commit window around the batch.
    pub(crate) fn dispatch_batch(&mut self, conn_id: u64, buf: &[u8]) -> BatchOutcome {
        let mut off = 0usize;
        loop {
            match parse_command_borrowed(&buf[off..]) {
                Ok(Some((argv, consumed))) => {
                    // on one shard every key is local; with more, most
                    // are not, and the owner prefetches those itself
                    if self.nshards == 1
                        && let Some(key) = argv.get(1)
                    {
                        self.store.prefetch_for_key(key);
                    }
                    self.handle_command(conn_id, &argv);
                    drop(argv);
                    off += consumed;
                    if crate::conn::conn_at(&mut self.conns, &mut self.conn_slot_hint, conn_id)
                        .is_none()
                    {
                        return BatchOutcome {
                            consumed: off,
                            protocol_error: false,
                            conn_gone: true,
                        };
                    }
                }
                Ok(None) => {
                    return BatchOutcome { consumed: off, protocol_error: false, conn_gone: false };
                }
                Err(_) => {
                    return BatchOutcome { consumed: off, protocol_error: true, conn_gone: false };
                }
            }
        }
    }

    /// Start fetching the keyspace bucket a forwarded request will probe.
    #[inline]
    pub(crate) fn prefetch_request(&self, argv: &crate::Argv, meta: DispatchMeta) {
        match (meta.key_hash, meta.key_idx) {
            (Some(h), _) => self.store.prefetch_for_hash(h),
            (None, Some(i)) => {
                if let Some(key) = argv.get(usize::from(i)) {
                    self.store.prefetch_for_key(key);
                }
            }
            (None, None) => {}
        }
    }
}
